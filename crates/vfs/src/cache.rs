//! On-disk cache of file content, filled on first access ("hydration").
//!
//! A cache file is named after the item and the version of its content, so
//! a remote change simply stops matching and the next open fetches again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use skydock_core::Provider;
use skydock_state::Item;

#[derive(Debug, thiserror::Error)]
pub enum HydrateError {
    #[error(transparent)]
    Provider(#[from] skydock_core::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// What arrived is not what the provider said it holds.
    #[error("downloaded data does not match the provider's {0}")]
    Corrupt(&'static str),
}

pub struct Cache {
    dir: PathBuf,
    /// One lock per file being fetched, so simultaneous opens of the same
    /// file share a single download.
    in_flight: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl Cache {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // Clear out what can never be used again: downloads cut short by a
        // crash, and files from before names carried the item part.
        for entry in std::fs::read_dir(dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(PARTIAL_SUFFIX) || !name.contains('-') {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    /// `<item>-<content version>`, both as hashes. The item part lets an
    /// outdated copy of the same file be found and removed.
    pub fn path_for(&self, item: &Item) -> PathBuf {
        let content_version = item.hash.as_deref().or(item.version.as_deref());
        self.dir.join(format!(
            "{}-{}",
            item_part(&item.id),
            &hex_digest(content_version.unwrap_or_default())[..16]
        ))
    }

    pub fn is_cached(&self, item: &Item) -> bool {
        self.path_for(item).is_file()
    }

    /// Make the item's content available locally and return where it is.
    /// The data goes to a partial file and only takes the final name once
    /// its size and hash match the provider's.
    pub async fn hydrate(
        &self,
        provider: &dyn Provider,
        item: &Item,
    ) -> Result<PathBuf, HydrateError> {
        let path = self.path_for(item);
        let lock = self
            .in_flight
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_default()
            .clone();
        let _fetching = lock.lock().await;

        let result = if path.is_file() {
            Ok(())
        } else {
            self.fetch(provider, item, &path).await
        };
        self.in_flight.lock().unwrap().remove(&path);
        result.map(|()| path)
    }

    async fn fetch(
        &self,
        provider: &dyn Provider,
        item: &Item,
        path: &Path,
    ) -> Result<(), HydrateError> {
        let partial = path.with_extension(&PARTIAL_SUFFIX[1..]);
        let outcome = async {
            let downloaded = provider.download(&item.id, &partial).await?;
            if item.size.is_some_and(|size| size != downloaded.bytes) {
                return Err(HydrateError::Corrupt("size"));
            }
            if item
                .hash
                .as_ref()
                .is_some_and(|hash| *hash != downloaded.hash)
            {
                return Err(HydrateError::Corrupt("hash"));
            }
            std::fs::rename(&partial, path)?;
            self.remove_older_copies(item, path);
            Ok(())
        }
        .await;
        if outcome.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        outcome
    }
}

impl Cache {
    /// Drop cached content of earlier versions of `item`. Anyone still
    /// reading one keeps their open file; only the name goes away.
    pub(crate) fn remove_older_copies(&self, item: &Item, current: &Path) {
        self.remove_copies(&item.id, Some(current));
    }

    /// Drop every cached version of an item that no longer exists.
    pub(crate) fn remove_all_copies(&self, item_id: &str) {
        self.remove_copies(item_id, None);
    }

    fn remove_copies(&self, item_id: &str, keep: Option<&Path>) {
        let prefix = format!("{}-", item_part(item_id));
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            if Some(entry.path().as_path()) != keep
                && entry.file_name().to_string_lossy().starts_with(&prefix)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

const PARTIAL_SUFFIX: &str = ".partial";

pub(crate) fn item_part(id: &str) -> String {
    hex_digest(id)[..32].to_owned()
}

fn hex_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// How much content is held on this device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub files: u64,
    pub bytes: u64,
}

/// What a cache directory holds. A missing directory holds nothing.
pub fn usage(dir: &Path) -> CacheUsage {
    let mut usage = CacheUsage::default();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(PARTIAL_SUFFIX)
        {
            continue;
        }
        if let Ok(metadata) = entry.metadata()
            && metadata.is_file()
        {
            usage.files += 1;
            usage.bytes += metadata.len();
        }
    }
    usage
}

/// Remove all downloaded content, returning what was freed. Nothing remote
/// is touched and files download again when next opened. Downloads still in
/// progress are left alone.
pub fn clear(dir: &Path) -> CacheUsage {
    let mut freed = CacheUsage::default();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(PARTIAL_SUFFIX)
        {
            continue;
        }
        if let Ok(metadata) = entry.metadata()
            && metadata.is_file()
            && std::fs::remove_file(entry.path()).is_ok()
        {
            freed.files += 1;
            freed.bytes += metadata.len();
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use skydock_core::{
        Account, ChangeSet, Downloaded, HashKind, PageCallback, ProviderKind, RemoteItem,
        UploadTarget, UrlCallback,
    };

    use super::*;

    /// Serves fixed bytes and counts how often it was asked.
    struct FakeProvider {
        content: &'static [u8],
        reported_hash: &'static str,
        downloads: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OneDrive
        }
        async fn sign_in(&self, _: &UrlCallback) -> skydock_core::Result<()> {
            unimplemented!()
        }
        async fn sign_out(&self) -> skydock_core::Result<()> {
            unimplemented!()
        }
        async fn is_signed_in(&self) -> skydock_core::Result<bool> {
            unimplemented!()
        }
        async fn account(&self) -> skydock_core::Result<Account> {
            unimplemented!()
        }
        async fn changes(
            &self,
            _: Option<&str>,
            _: &PageCallback<'_>,
        ) -> skydock_core::Result<ChangeSet> {
            unimplemented!()
        }
        async fn top_level(&self) -> skydock_core::Result<Vec<RemoteItem>> {
            unimplemented!()
        }
        fn hash_kind(&self) -> HashKind {
            HashKind::Md5
        }
        async fn download(&self, _: &str, dest: &Path) -> skydock_core::Result<Downloaded> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            tokio::fs::write(dest, self.content).await?;
            Ok(Downloaded {
                bytes: self.content.len() as u64,
                hash: self.reported_hash.to_owned(),
            })
        }
        fn accepts_name(&self, _: &str) -> bool {
            unimplemented!()
        }
        fn names_are_case_sensitive(&self) -> bool {
            unimplemented!()
        }
        async fn upload(&self, _: UploadTarget<'_>, _: &Path) -> skydock_core::Result<RemoteItem> {
            unimplemented!()
        }
        async fn create_folder(&self, _: &str, _: &str) -> skydock_core::Result<RemoteItem> {
            unimplemented!()
        }
        async fn move_item(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> skydock_core::Result<RemoteItem> {
            unimplemented!()
        }
        async fn delete(&self, _: &str) -> skydock_core::Result<()> {
            unimplemented!()
        }
    }

    fn provider(content: &'static [u8]) -> FakeProvider {
        FakeProvider {
            content,
            reported_hash: "h1",
            downloads: AtomicUsize::new(0),
        }
    }

    fn item(size: u64, hash: &str) -> Item {
        Item {
            id: "id".to_owned(),
            name: "a.txt".to_owned(),
            is_folder: false,
            size: Some(size),
            version: None,
            hash: Some(hash.to_owned()),
            modified: None,
        }
    }

    fn cache(test: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("skydock-cache-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::new(&dir).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_opens_share_one_download_and_later_opens_need_none() {
        let (cache, provider, item) = (cache("share"), provider(b"hello"), item(5, "h1"));
        let (a, b) = tokio::join!(
            cache.hydrate(&provider, &item),
            cache.hydrate(&provider, &item)
        );
        let path = a.unwrap();
        assert_eq!(path, b.unwrap());
        cache.hydrate(&provider, &item).await.unwrap();

        assert_eq!(provider.downloads.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(cache.is_cached(&item));
    }

    #[tokio::test]
    async fn mismatching_data_is_rejected_and_leaves_nothing_behind() {
        let (cache, provider) = (cache("corrupt"), provider(b"hello"));
        for bad in [item(4, "h1"), item(5, "other")] {
            let result = cache.hydrate(&provider, &bad).await;
            assert!(matches!(result, Err(HydrateError::Corrupt(_))));
            assert!(!cache.is_cached(&bad));
        }
        assert_eq!(std::fs::read_dir(&cache.dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_new_version_replaces_the_old_copy_and_clearing_frees_everything() {
        let (cache, provider) = (cache("replace"), provider(b"hello"));
        let old = cache.hydrate(&provider, &item(5, "h1")).await.unwrap();
        // The file changes remotely; the provider now reports another hash.
        let provider = FakeProvider {
            reported_hash: "h2",
            ..provider
        };
        let new = cache.hydrate(&provider, &item(5, "h2")).await.unwrap();
        assert!(!old.exists() && new.exists());
        assert_eq!(usage(&cache.dir), CacheUsage { files: 1, bytes: 5 });

        assert_eq!(clear(&cache.dir), CacheUsage { files: 1, bytes: 5 });
        assert_eq!(usage(&cache.dir), CacheUsage::default());
        assert_eq!(
            usage(Path::new("/nonexistent/skydock")),
            CacheUsage::default()
        );
    }

    #[test]
    fn opening_a_cache_discards_unfinished_and_legacy_files() {
        let cache = cache("startup");
        let keep = cache.path_for(&item(5, "h1"));
        std::fs::write(&keep, b"hello").unwrap();
        std::fs::write(cache.dir.join("abc-def.partial"), b"he").unwrap();
        std::fs::write(cache.dir.join("0123456789abcdef"), b"old naming").unwrap();

        Cache::new(&cache.dir).unwrap();
        assert_eq!(usage(&cache.dir), CacheUsage { files: 1, bytes: 5 });
        assert!(keep.exists());
    }

    #[test]
    fn changed_content_gets_a_different_cache_file() {
        let cache = cache("version");
        assert_ne!(
            cache.path_for(&item(5, "h1")),
            cache.path_for(&item(5, "h2"))
        );
        assert_eq!(
            cache.path_for(&item(5, "h1")),
            cache.path_for(&item(9, "h1"))
        );
    }
}
