//! End-to-end tests: a real mount, driven through ordinary file operations,
//! in front of a provider that lives in memory. They need `fusermount3` and
//! are skipped where it is not installed.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use skydock_core::hash::{HashKind, Hasher};
use skydock_core::{
    Account, Change, ChangeSet, Downloaded, Error, PageCallback, Provider, ProviderKind,
    RemoteItem, UploadTarget, UrlCallback,
};
use skydock_state::Store;

use crate::{Mount, mount};

const ACCOUNT: &str = "fake:1";

#[derive(Clone)]
struct Entry {
    parent: Option<String>,
    name: String,
    /// `None` for a folder.
    content: Option<Vec<u8>>,
    version: u64,
}

#[derive(Default)]
struct Remote {
    entries: HashMap<String, Entry>,
    next_id: u64,
    downloads: usize,
    /// Names uploaded as new files, in order.
    created: Vec<String>,
    /// While set, every upload fails as if the network were down.
    offline: bool,
}

/// A drive in memory. Names are case-insensitive and may not contain `:`,
/// like OneDrive's; a new file whose name is taken gets ` 1` appended.
#[derive(Default)]
struct FakeDrive {
    remote: Mutex<Remote>,
}

fn md5(content: &[u8]) -> String {
    let mut hasher = Hasher::new(HashKind::Md5);
    hasher.update(content);
    hasher.finish()
}

fn api_error(status: u16) -> Error {
    Error::Api {
        status: reqwest::StatusCode::from_u16(status).unwrap(),
        body: String::new(),
    }
}

impl Remote {
    fn describe(&self, id: &str) -> RemoteItem {
        let entry = &self.entries[id];
        RemoteItem {
            id: id.to_owned(),
            parent_id: entry.parent.clone(),
            name: entry.name.clone(),
            is_folder: entry.content.is_none(),
            size: entry.content.as_ref().map(|content| content.len() as u64),
            version: Some(format!("v{}", entry.version)),
            hash: entry.content.as_deref().map(md5),
            modified: Some("2026-01-01T00:00:00Z".to_owned()),
        }
    }

    fn add(&mut self, parent: Option<&str>, name: &str, content: Option<&[u8]>) -> String {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        self.entries.insert(
            id.clone(),
            Entry {
                parent: parent.map(str::to_owned),
                name: name.to_owned(),
                content: content.map(<[u8]>::to_vec),
                version: 1,
            },
        );
        id
    }

    fn named(&self, parent: &str, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(_, entry)| {
                entry.parent.as_deref() == Some(parent)
                    && entry.name.to_lowercase() == name.to_lowercase()
            })
            .map(|(id, _)| id.clone())
    }
}

impl FakeDrive {
    /// Root, `Docs/`, `a.txt` ("hello") and `Docs/b.txt` ("world").
    fn seeded() -> Self {
        let mut remote = Remote::default();
        let root = remote.add(None, "root", None);
        let docs = remote.add(Some(&root), "Docs", None);
        remote.add(Some(&root), "a.txt", Some(b"hello"));
        remote.add(Some(&docs), "b.txt", Some(b"world"));
        Self {
            remote: Mutex::new(remote),
        }
    }

    fn listing(&self) -> ChangeSet {
        let remote = self.remote.lock().unwrap();
        ChangeSet {
            changes: remote
                .entries
                .keys()
                .map(|id| Change::Upsert(remote.describe(id)))
                .collect(),
            cursor: "cursor".to_owned(),
            full: true,
        }
    }

    /// The entry at a `/`-separated path from the root.
    fn at(&self, path: &str) -> Option<(String, Entry)> {
        let remote = self.remote.lock().unwrap();
        let mut id = remote
            .entries
            .iter()
            .find(|(_, entry)| entry.parent.is_none())
            .map(|(id, _)| id.clone())?;
        for name in path.split('/').filter(|part| !part.is_empty()) {
            id = remote
                .entries
                .iter()
                .find(|(_, entry)| entry.parent.as_deref() == Some(&id) && entry.name == name)
                .map(|(id, _)| id.clone())?;
        }
        let entry = remote.entries[&id].clone();
        Some((id, entry))
    }

    fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.at(path)?.1.content
    }

    /// Somebody else edits a file.
    fn edit_elsewhere(&self, path: &str, content: &[u8]) {
        let (id, _) = self.at(path).unwrap();
        let mut remote = self.remote.lock().unwrap();
        let entry = remote.entries.get_mut(&id).unwrap();
        entry.content = Some(content.to_vec());
        entry.version += 1;
    }
}

#[async_trait]
impl Provider for FakeDrive {
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
        Ok(self.listing())
    }
    async fn top_level(&self) -> skydock_core::Result<Vec<RemoteItem>> {
        unimplemented!()
    }
    fn hash_kind(&self) -> HashKind {
        HashKind::Md5
    }

    async fn download(&self, item_id: &str, dest: &Path) -> skydock_core::Result<Downloaded> {
        let content = {
            let mut remote = self.remote.lock().unwrap();
            remote.downloads += 1;
            let entry = remote.entries.get(item_id).ok_or_else(|| api_error(404))?;
            entry.content.clone().ok_or_else(|| api_error(400))?
        };
        tokio::fs::write(dest, &content).await?;
        Ok(Downloaded {
            bytes: content.len() as u64,
            hash: md5(&content),
        })
    }

    fn accepts_name(&self, name: &str) -> bool {
        !name.contains(':')
    }

    fn names_are_case_sensitive(&self) -> bool {
        false
    }

    async fn upload(
        &self,
        target: UploadTarget<'_>,
        source: &Path,
    ) -> skydock_core::Result<RemoteItem> {
        let content = tokio::fs::read(source).await?;
        let mut remote = self.remote.lock().unwrap();
        if remote.offline {
            return Err(api_error(500));
        }
        let id = match target {
            UploadTarget::New { parent_id, name } => {
                if !remote.entries.contains_key(parent_id) {
                    return Err(api_error(404));
                }
                let name = match remote.named(parent_id, name) {
                    Some(_) => format!("{name} 1"),
                    None => name.to_owned(),
                };
                remote.created.push(name.clone());
                remote.add(Some(parent_id), &name, Some(&content))
            }
            UploadTarget::Replace {
                item_id,
                base_version,
                base_hash,
            } => {
                let current = remote.describe_existing(item_id)?;
                if current.version.as_deref() != base_version
                    && current.hash.as_deref() != base_hash
                {
                    return Err(Error::Conflict);
                }
                let entry = remote.entries.get_mut(item_id).unwrap();
                entry.content = Some(content);
                entry.version += 1;
                item_id.to_owned()
            }
        };
        Ok(remote.describe(&id))
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> skydock_core::Result<RemoteItem> {
        let mut remote = self.remote.lock().unwrap();
        if remote.named(parent_id, name).is_some() {
            return Err(api_error(409));
        }
        let id = remote.add(Some(parent_id), name, None);
        Ok(remote.describe(&id))
    }

    async fn move_item(
        &self,
        item_id: &str,
        _from_parent_id: &str,
        to_parent_id: &str,
        name: &str,
    ) -> skydock_core::Result<RemoteItem> {
        let mut remote = self.remote.lock().unwrap();
        remote.describe_existing(item_id)?;
        if remote
            .named(to_parent_id, name)
            .is_some_and(|other| other != item_id)
        {
            return Err(api_error(409));
        }
        let entry = remote.entries.get_mut(item_id).unwrap();
        entry.parent = Some(to_parent_id.to_owned());
        entry.name = name.to_owned();
        entry.version += 1;
        Ok(remote.describe(item_id))
    }

    async fn delete(&self, item_id: &str) -> skydock_core::Result<()> {
        let mut remote = self.remote.lock().unwrap();
        remote.entries.remove(item_id);
        remote
            .entries
            .retain(|_, entry| entry.parent.as_deref() != Some(item_id));
        Ok(())
    }
}

impl Remote {
    fn describe_existing(&self, id: &str) -> skydock_core::Result<RemoteItem> {
        match self.entries.contains_key(id) {
            true => Ok(self.describe(id)),
            false => Err(api_error(404)),
        }
    }
}

/// A mounted fake drive in its own temporary directory.
struct Fixture {
    drive: Arc<FakeDrive>,
    dir: PathBuf,
    mount: Option<Mount>,
}

impl Fixture {
    /// `None` where FUSE mounts are not possible.
    fn new(test: &str) -> Option<Self> {
        let installed = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("fusermount3").is_file())
        });
        if !installed {
            eprintln!("skipped: fusermount3 is not installed");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("skydock-fs-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let drive = Arc::new(FakeDrive::seeded());
        Store::open(&dir.join("state.sqlite"))
            .unwrap()
            .apply(ACCOUNT, drive.listing())
            .unwrap();
        let mut fixture = Self {
            drive,
            dir,
            mount: None,
        };
        fixture.mount();
        Some(fixture)
    }

    fn mount(&mut self) {
        self.mount = Some(
            mount(
                Arc::clone(&self.drive) as Arc<dyn Provider>,
                self.store(),
                ACCOUNT.to_owned(),
                &self.dir.join("mnt"),
                &self.dir.join("cache"),
                &self.dir.join("pending"),
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
    }

    fn unmount(&mut self) {
        self.mount = None;
    }

    fn store(&self) -> Store {
        Store::open(&self.dir.join("state.sqlite")).unwrap()
    }

    /// A path inside the mounted folder.
    fn path(&self, relative: &str) -> PathBuf {
        self.dir.join("mnt").join(relative)
    }

    fn waiting(&self) -> usize {
        self.store().dirty(ACCOUNT).unwrap().len()
    }

    fn pending_files(&self) -> usize {
        std::fs::read_dir(self.dir.join("pending")).unwrap().count()
    }

    fn downloads(&self) -> usize {
        self.drive.remote.lock().unwrap().downloads
    }

    fn created(&self) -> Vec<String> {
        self.drive.remote.lock().unwrap().created.clone()
    }

    /// Wait for background uploads to reach a state.
    fn eventually(&self, what: &str, reached: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reached(self) {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn settled(&self) {
        self.eventually("nothing waits for upload", |f| f.waiting() == 0);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.mount = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn errno(result: std::io::Result<impl Sized>) -> Option<i32> {
    result.err().and_then(|error| error.raw_os_error())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_file_is_uploaded_after_it_is_closed() {
    let Some(f) = Fixture::new("create") else {
        return;
    };
    std::fs::write(f.path("Docs/new.txt"), b"fresh").unwrap();
    assert_eq!(std::fs::read(f.path("Docs/new.txt")).unwrap(), b"fresh");

    f.settled();
    assert_eq!(f.drive.content("Docs/new.txt").unwrap(), b"fresh");
    let (id, _) = f.drive.at("Docs/new.txt").unwrap();
    assert_eq!(
        f.store().path(ACCOUNT, &id).unwrap().as_deref(),
        Some("/Docs/new.txt")
    );
    assert_eq!(f.pending_files(), 0);

    // What was uploaded doubles as the downloaded copy.
    assert_eq!(std::fs::read(f.path("Docs/new.txt")).unwrap(), b"fresh");
    assert_eq!(std::fs::metadata(f.path("Docs/new.txt")).unwrap().len(), 5);
    assert_eq!(f.downloads(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editing_a_file_fetches_it_once_and_replaces_it_remotely() {
    let Some(f) = Fixture::new("edit") else {
        return;
    };
    let mut file = std::fs::File::options()
        .append(true)
        .open(f.path("a.txt"))
        .unwrap();
    file.write_all(b", again").unwrap();
    // Visible at once, before any upload.
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"hello, again");
    assert_eq!(std::fs::metadata(f.path("a.txt")).unwrap().len(), 12);
    assert_eq!(
        f.drive.content("a.txt").unwrap(),
        b"hello",
        "not while it is open"
    );
    drop(file);

    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"hello, again");
    assert_eq!(f.drive.at("a.txt").unwrap().1.version, 2);
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"hello, again");
    assert_eq!(f.downloads(), 1);
    assert!(f.created().is_empty(), "the file was replaced, not added");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwriting_a_file_does_not_fetch_what_is_being_thrown_away() {
    let Some(f) = Fixture::new("overwrite") else {
        return;
    };
    std::fs::write(f.path("a.txt"), b"entirely new").unwrap();
    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"entirely new");
    assert_eq!(f.downloads(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_for_writing_without_writing_changes_nothing() {
    let Some(f) = Fixture::new("untouched") else {
        return;
    };
    drop(
        std::fs::File::options()
            .write(true)
            .open(f.path("a.txt"))
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.waiting(), 0);
    assert_eq!(f.drive.at("a.txt").unwrap().1.version, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncating_keeps_the_start_of_the_file() {
    let Some(f) = Fixture::new("truncate") else {
        return;
    };
    let file = std::fs::File::options()
        .write(true)
        .open(f.path("a.txt"))
        .unwrap();
    file.set_len(2).unwrap();
    file.write_all_at(b"y!", 1).unwrap();
    drop(file);
    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"hy!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folders_renames_and_deletions_happen_remotely_at_once() {
    let Some(f) = Fixture::new("structure") else {
        return;
    };
    std::fs::create_dir(f.path("Docs/Work")).unwrap();
    assert!(f.drive.at("Docs/Work").is_some());
    assert_eq!(
        errno(std::fs::create_dir(f.path("docs"))),
        Some(nix::libc::EEXIST)
    );

    std::fs::rename(f.path("Docs/b.txt"), f.path("Docs/Work/c.txt")).unwrap();
    assert_eq!(f.drive.content("Docs/Work/c.txt").unwrap(), b"world");
    assert!(f.drive.at("Docs/b.txt").is_none());
    assert_eq!(std::fs::read(f.path("Docs/Work/c.txt")).unwrap(), b"world");

    std::fs::rename(f.path("Docs"), f.path("Papers")).unwrap();
    assert!(f.drive.at("Papers/Work/c.txt").is_some());

    assert_eq!(
        errno(std::fs::remove_dir(f.path("Papers/Work"))),
        Some(nix::libc::ENOTEMPTY)
    );
    std::fs::remove_file(f.path("Papers/Work/c.txt")).unwrap();
    std::fs::remove_dir(f.path("Papers/Work")).unwrap();
    assert!(f.drive.at("Papers/Work").is_none());
    assert!(!f.path("Papers/Work").exists());

    // Replacing an existing file by renaming another onto it.
    std::fs::write(f.path("Papers/d.txt"), b"d").unwrap();
    f.settled();
    std::fs::rename(f.path("Papers/d.txt"), f.path("a.txt")).unwrap();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"d");
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"d");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saving_through_a_temporary_file_updates_the_document_itself() {
    let Some(f) = Fixture::new("atomic") else {
        return;
    };
    let (document, _) = f.drive.at("a.txt").unwrap();

    // Held open across the rename so the upload cannot start in between.
    let mut temporary = std::fs::File::create(f.path("a.txt.X1Y2Z3")).unwrap();
    temporary.write_all(b"saved").unwrap();
    std::fs::rename(f.path("a.txt.X1Y2Z3"), f.path("a.txt")).unwrap();
    drop(temporary);
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"saved");

    f.settled();
    let (id, entry) = f.drive.at("a.txt").unwrap();
    assert_eq!((id, entry.content.unwrap()), (document, b"saved".to_vec()));
    assert!(f.created().is_empty(), "the temporary file never went up");
    assert!(!f.path("a.txt.X1Y2Z3").exists());
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"saved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_elsewhere_too_is_kept_beside_the_other_version() {
    let Some(f) = Fixture::new("conflict") else {
        return;
    };
    let mut file = std::fs::File::options()
        .append(true)
        .open(f.path("a.txt"))
        .unwrap();
    file.write_all(b" from here").unwrap();
    f.drive.edit_elsewhere("a.txt", b"from elsewhere");
    drop(file);

    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"from elsewhere");
    let copies = f.created();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].starts_with("a (conflicted copy from ") && copies[0].ends_with(").txt"));
    assert_eq!(f.drive.content(&copies[0]).unwrap(), b"hello from here");
    assert_eq!(
        std::fs::read(f.path(&copies[0])).unwrap(),
        b"hello from here"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_deleted_elsewhere_while_edited_here_is_put_back() {
    let Some(f) = Fixture::new("resurrect") else {
        return;
    };
    let mut file = std::fs::File::options()
        .append(true)
        .open(f.path("a.txt"))
        .unwrap();
    file.write_all(b"!").unwrap();
    let (id, _) = f.drive.at("a.txt").unwrap();
    f.drive.delete(&id).await.unwrap();
    drop(file);

    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"hello!");
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"hello!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scratch_files_stay_local_until_given_a_real_name() {
    let Some(f) = Fixture::new("transient") else {
        return;
    };
    std::fs::write(f.path(".~lock.a.txt#"), b"lock").unwrap();
    std::fs::write(f.path("draft.tmp"), b"draft").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(f.created().is_empty());
    assert_eq!(std::fs::read(f.path("draft.tmp")).unwrap(), b"draft");

    std::fs::remove_file(f.path(".~lock.a.txt#")).unwrap();
    std::fs::rename(f.path("draft.tmp"), f.path("final.txt")).unwrap();
    f.settled();
    assert_eq!(f.created(), ["final.txt"]);
    assert_eq!(f.drive.content("final.txt").unwrap(), b"draft");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_the_provider_cannot_store_are_refused() {
    let Some(f) = Fixture::new("names") else {
        return;
    };
    assert_eq!(
        errno(std::fs::write(f.path("a:b.txt"), b"x")),
        Some(nix::libc::EINVAL)
    );
    assert_eq!(
        errno(std::fs::File::create_new(f.path("A.TXT"))),
        Some(nix::libc::EEXIST)
    );
    assert_eq!(
        errno(std::fs::create_dir(f.path(".Trash-1000"))),
        Some(nix::libc::EPERM)
    );
    assert_eq!(
        errno(std::fs::rename(f.path("a.txt"), f.path("b:c"))),
        Some(nix::libc::EINVAL)
    );
    assert_eq!(f.waiting(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_made_offline_survive_a_restart_and_a_full_resync() {
    let Some(mut f) = Fixture::new("offline") else {
        return;
    };
    f.drive.remote.lock().unwrap().offline = true;
    std::fs::write(f.path("Docs/offline.txt"), b"kept").unwrap();
    std::fs::write(f.path("a.txt"), b"edited offline").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.waiting(), 2);

    f.unmount();
    // A full listing knows nothing of the new file and has the old a.txt.
    f.store().apply(ACCOUNT, f.drive.listing()).unwrap();
    assert_eq!(f.waiting(), 2);

    f.mount();
    assert_eq!(std::fs::read(f.path("Docs/offline.txt")).unwrap(), b"kept");
    assert_eq!(std::fs::read(f.path("a.txt")).unwrap(), b"edited offline");

    f.unmount();
    f.drive.remote.lock().unwrap().offline = false;
    f.mount();
    f.settled();
    assert_eq!(f.drive.content("Docs/offline.txt").unwrap(), b"kept");
    assert_eq!(f.drive.content("a.txt").unwrap(), b"edited offline");
    assert_eq!(f.pending_files(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_command_line_tools_work() {
    let Some(f) = Fixture::new("tools") else {
        return;
    };
    let outside = f.dir.join("outside.txt");
    std::fs::write(&outside, b"from outside\n").unwrap();
    // Copy in keeping times and mode, create empty, append, edit in place
    // (through a temporary file), move within the folder, ask for free space.
    let script = r#"
        set -e
        cp -p "$1" copied.txt
        touch empty.txt
        echo ' more' >> a.txt
        sed -i 's/hello/HELLO/' a.txt
        mv copied.txt Docs/
        test "$(df --output=avail . | tail -1)" -gt 0
    "#;
    let output = std::process::Command::new("sh")
        .args(["-c", script, "sh"])
        .arg(&outside)
        .current_dir(f.path(""))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    f.settled();
    assert_eq!(f.drive.content("a.txt").unwrap(), b"HELLO more\n");
    assert_eq!(f.drive.content("empty.txt").unwrap(), b"");
    assert_eq!(
        f.drive.content("Docs/copied.txt").unwrap(),
        b"from outside\n"
    );
    assert!(f.drive.at("copied.txt").is_none());
    assert_eq!(f.pending_files(), 0);
}
