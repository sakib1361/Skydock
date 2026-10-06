//! Application logic shared by every front end (CLI, GUI, later the
//! daemon): settings, constructing providers, and the operations on them.

mod builtin;
mod settings;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use skydock_core::{Account, ProgressCallback, Provider, ProviderKind, UrlCallback};
use skydock_gdrive::GoogleDrive;
use skydock_onedrive::OneDrive;
use skydock_state::{Applied, Store, Totals};

pub use skydock_state::Item;
pub use skydock_vfs::{CacheUsage, Mount};

pub use settings::Settings;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Provider(#[from] skydock_core::Error),

    #[error(transparent)]
    State(#[from] skydock_state::Error),

    #[error(transparent)]
    Mount(#[from] skydock_vfs::MountError),

    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },

    #[error("invalid settings file {path}: {message}")]
    Settings { path: PathBuf, message: String },

    #[error("{0}: no such file or folder in the fetched file list")]
    NotFound(String),

    #[error("{0} is a folder")]
    IsFolder(String),

    #[error("{0} has no downloadable content")]
    NoContent(String),

    /// What arrived is not what the provider said it holds. The partial
    /// file has been removed.
    #[error("{path}: downloaded data does not match the provider's {what}")]
    Corrupt { path: String, what: &'static str },

    #[error("cannot determine the user's {0} directory")]
    NoUserDir(&'static str),
}

#[derive(Debug)]
pub struct PullReport {
    pub applied: Applied,
    /// The whole drive was enumerated rather than only its changes.
    pub full: bool,
    pub totals: Totals,
}

#[derive(Debug)]
pub struct DownloadReport {
    pub dest: PathBuf,
    pub bytes: u64,
    /// The content hash was compared with the provider's. `false` only when
    /// the provider publishes no hash for the item.
    pub hash_checked: bool,
}

#[derive(Debug)]
pub enum ProviderStatus {
    /// No client ID has been set for this provider.
    NotConfigured,
    SignedOut,
    SignedIn {
        account: Account,
        /// `None` until the first pull.
        totals: Option<Totals>,
    },
}

pub struct Service {
    settings: Settings,
    state_path: PathBuf,
}

impl Service {
    pub fn load() -> Result<Self> {
        let state_path = dirs::data_dir()
            .ok_or(Error::NoUserDir("data"))?
            .join("skydock/state.sqlite");
        Ok(Self {
            settings: Settings::load()?,
            state_path,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// Where this provider's files live locally: its own folder directly
    /// inside the sync folder, never shared with another provider.
    pub fn provider_folder(&self, kind: ProviderKind) -> PathBuf {
        self.settings.sync_root().join(kind.folder_name())
    }

    /// The single place that knows which crate implements which provider.
    pub fn provider(&self, kind: ProviderKind) -> Result<Box<dyn Provider>> {
        let not_configured = || skydock_core::Error::NotConfigured(kind.display_name().to_owned());
        Ok(match kind {
            ProviderKind::OneDrive => {
                let client_id = self
                    .settings
                    .onedrive_client_id()
                    .ok_or_else(not_configured)?;
                Box::new(OneDrive::new(client_id)?)
            }
            ProviderKind::GoogleDrive => {
                let (client_id, client_secret) = self
                    .settings
                    .gdrive_credentials()
                    .ok_or_else(not_configured)?;
                Box::new(GoogleDrive::new(client_id, client_secret)?)
            }
        })
    }

    pub fn is_configured(&self, kind: ProviderKind) -> bool {
        match kind {
            ProviderKind::OneDrive => self.settings.onedrive_client_id().is_some(),
            ProviderKind::GoogleDrive => self.settings.gdrive_credentials().is_some(),
        }
    }

    pub async fn sign_in(&self, kind: ProviderKind, on_url: &UrlCallback) -> Result<Account> {
        let provider = self.provider(kind)?;
        provider.sign_in(on_url).await?;
        let account = provider.account().await?;
        Store::open(&self.state_path)?.set_known_account(kind.id(), Some(&account.id))?;
        Ok(account)
    }

    pub async fn sign_out(&self, kind: ProviderKind) -> Result<()> {
        self.provider(kind)?.sign_out().await?;
        Store::open(&self.state_path)?.set_known_account(kind.id(), None)?;
        Ok(())
    }

    /// Bring the metadata mirror up to date: a full enumeration on first
    /// run or when `full` is set, changes only otherwise.
    pub async fn pull(
        &self,
        kind: ProviderKind,
        full: bool,
        progress: &ProgressCallback,
    ) -> Result<PullReport> {
        let provider = self.provider(kind)?;
        let key = self.account_key(kind, &*provider).await?;
        let cursor = match full {
            true => None,
            false => Store::open(&self.state_path)?.cursor(&key)?,
        };

        let set = provider.changes(cursor.as_deref(), progress).await?;
        let full = set.full;

        let mut store = Store::open(&self.state_path)?;
        let applied = store.apply(&key, set)?;
        Ok(PullReport {
            applied,
            full,
            totals: store.totals(&key)?,
        })
    }

    /// What the fetched file list holds at `path`: a folder's contents, or
    /// the single file.
    pub async fn list(&self, kind: ProviderKind, path: &str) -> Result<Vec<Item>> {
        let key = self.account_key(kind, &*self.provider(kind)?).await?;
        let store = Store::open(&self.state_path)?;
        let item = store
            .resolve(&key, path)?
            .ok_or_else(|| Error::NotFound(path.to_owned()))?;
        Ok(if item.is_folder {
            store.children(&key, &item.id)?
        } else {
            vec![item]
        })
    }

    /// Download the file at `path` to `dest`, or to its place inside the
    /// provider's folder when `dest` is `None`. The data goes to a partial
    /// file first and only replaces `dest` once size and hash check out.
    pub async fn download(
        &self,
        kind: ProviderKind,
        path: &str,
        dest: Option<&Path>,
    ) -> Result<DownloadReport> {
        let provider = self.provider(kind)?;
        let key = self.account_key(kind, &*provider).await?;
        let item = Store::open(&self.state_path)?
            .resolve(&key, path)?
            .ok_or_else(|| Error::NotFound(path.to_owned()))?;
        if item.is_folder {
            return Err(Error::IsFolder(path.to_owned()));
        }
        if item.size.is_none() && item.hash.is_none() {
            return Err(Error::NoContent(path.to_owned()));
        }

        let dest = match dest {
            Some(dest) => dest.to_owned(),
            None => self
                .provider_folder(kind)
                .join(path.trim_start_matches('/')),
        };
        let io = |context: String| move |source| Error::Io { context, source };
        let folder = dest.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(folder)
            .map_err(io(format!("cannot create {}", folder.display())))?;
        let partial = folder.join(format!(".{}.skydock-partial", item.name));

        let outcome = async {
            let downloaded = provider.download(&item.id, &partial).await?;
            let corrupt = |what| Error::Corrupt {
                path: path.to_owned(),
                what,
            };
            if item.size.is_some_and(|size| size != downloaded.bytes) {
                return Err(corrupt("size"));
            }
            if item
                .hash
                .as_ref()
                .is_some_and(|hash| *hash != downloaded.hash)
            {
                return Err(corrupt("hash"));
            }
            std::fs::rename(&partial, &dest)
                .map_err(io(format!("cannot write {}", dest.display())))?;
            Ok(downloaded.bytes)
        }
        .await;

        match outcome {
            Ok(bytes) => Ok(DownloadReport {
                dest,
                bytes,
                hash_checked: item.hash.is_some(),
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&partial);
                Err(error)
            }
        }
    }

    pub async fn status(&self, kind: ProviderKind) -> Result<ProviderStatus> {
        if !self.is_configured(kind) {
            return Ok(ProviderStatus::NotConfigured);
        }
        let provider = self.provider(kind)?;
        if !provider.is_signed_in().await? {
            return Ok(ProviderStatus::SignedOut);
        }
        let account = provider.account().await?;
        let key = format!("{}:{}", kind.id(), account.id);
        let store = Store::open(&self.state_path)?;
        store.set_known_account(kind.id(), Some(&account.id))?;
        let totals = match store.cursor(&key)? {
            Some(_) => Some(store.totals(&key)?),
            None => None,
        };
        Ok(ProviderStatus::SignedIn { account, totals })
    }

    /// Mount the provider's drive at its folder as files on demand. Needs
    /// a fetched file list but no network; downloads run on `runtime`.
    pub async fn mount(
        &self,
        kind: ProviderKind,
        runtime: tokio::runtime::Handle,
    ) -> Result<Mount> {
        let provider: Arc<dyn Provider> = Arc::from(self.provider(kind)?);
        let key = self.account_key(kind, &*provider).await?;
        let cache_dir = cache_dir(&key)?;
        Ok(skydock_vfs::mount(
            provider,
            Store::open(&self.state_path)?,
            key,
            &self.provider_folder(kind),
            &cache_dir,
            runtime,
        )?)
    }

    /// How much of this provider's content has been downloaded to this
    /// device. Nothing, if no account is on record.
    pub fn on_device(&self, kind: ProviderKind) -> Result<CacheUsage> {
        Ok(match self.known_cache_dir(kind)? {
            Some(dir) => skydock_vfs::cache_usage(&dir),
            None => CacheUsage::default(),
        })
    }

    /// Remove this provider's downloaded content from the device and say
    /// what was freed. Nothing in the cloud changes; files download again
    /// when opened.
    pub fn free_up_space(&self, kind: ProviderKind) -> Result<CacheUsage> {
        Ok(match self.known_cache_dir(kind)? {
            Some(dir) => skydock_vfs::clear_cache(&dir),
            None => CacheUsage::default(),
        })
    }

    fn known_cache_dir(&self, kind: ProviderKind) -> Result<Option<PathBuf>> {
        Store::open(&self.state_path)?
            .known_account(kind.id())?
            .map(|account_id| cache_dir(&format!("{}:{account_id}", kind.id())))
            .transpose()
    }

    /// Key under which an account's items live in the state database.
    /// Includes the provider so IDs from different services cannot collide.
    /// Asks the provider only if the account is not on record yet.
    async fn account_key(&self, kind: ProviderKind, provider: &dyn Provider) -> Result<String> {
        let store = Store::open(&self.state_path)?;
        let account_id = match store.known_account(kind.id())? {
            Some(id) => id,
            None => {
                drop(store);
                let id = provider.account().await?.id;
                Store::open(&self.state_path)?.set_known_account(kind.id(), Some(&id))?;
                id
            }
        };
        Ok(format!("{}:{account_id}", kind.id()))
    }
}

/// Where an account's downloaded content is kept.
fn cache_dir(account_key: &str) -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .ok_or(Error::NoUserDir("cache"))?
        .join("skydock")
        .join(account_key.replace([':', '/'], "-")))
}
