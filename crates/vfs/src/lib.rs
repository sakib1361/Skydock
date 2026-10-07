//! Files on demand: mounts one account's drive as a folder whose files
//! download when first opened and upload after they are changed.

mod cache;
mod fs;
mod names;
mod pending;
#[cfg(test)]
mod tests;
mod upload;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuser::MountOption;
use skydock_core::Provider;
use skydock_state::Store;
use tokio::runtime::Handle;

pub use cache::{CacheUsage, HydrateError, clear as clear_cache, usage as cache_usage};

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("{0} is already a mount point; is Skydock already running?")]
    AlreadyMounted(PathBuf),

    /// Mounting would hide whatever is in the folder.
    #[error("{0} is not empty; move its contents elsewhere first")]
    NotEmpty(PathBuf),

    /// Ubuntu's AppArmor profile for `fusermount3` only permits mounts in
    /// a few places; elsewhere the mount is refused whatever the folder's
    /// own permissions say.
    #[error(
        "the system does not allow an on-demand folder at {0}. Choose a sync folder inside \
         your home folder (or under /mnt or /media)"
    )]
    Refused(PathBuf),

    #[error("no file list has been fetched for this account yet")]
    NothingFetched,

    #[error(transparent)]
    State(#[from] skydock_state::Error),

    #[error("cannot mount {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// A live mount. Dropping it unmounts.
pub struct Mount {
    mountpoint: PathBuf,
    _session: fuser::BackgroundSession,
}

impl Mount {
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

/// Mount `account`'s drive at `mountpoint`. `store` must be a connection of
/// its own; transfers run on `runtime`. Downloads land in `cache_dir`, which
/// may be emptied at any time; changes not uploaded yet are kept in
/// `pending_dir`, which must not be.
pub fn mount(
    provider: Arc<dyn Provider>,
    store: Store,
    account: String,
    mountpoint: &Path,
    cache_dir: &Path,
    pending_dir: &Path,
    runtime: Handle,
) -> Result<Mount, MountError> {
    let io = |source| MountError::Io {
        path: mountpoint.to_owned(),
        source,
    };
    let root = store.root(&account)?.ok_or(MountError::NothingFetched)?;
    prepare_mountpoint(mountpoint)?;
    let owner = std::fs::metadata(mountpoint).map_err(io)?;
    let cache = cache::Cache::new(cache_dir).map_err(io)?;
    let pending = pending::Pending::new(pending_dir).map_err(io)?;

    let filesystem = fs::SkydockFs::new(
        account,
        store,
        &root,
        Arc::clone(&provider),
        cache,
        pending,
        runtime,
        (owner.uid(), owner.gid()),
    );
    filesystem.inner().resume_uploads()?;
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::FSName("skydock".to_owned()),
        MountOption::Subtype(provider.kind().id().to_owned()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    let session = fuser::spawn_mount(filesystem, mountpoint, &config).map_err(|source| {
        if source.to_string().contains("Permission denied") {
            MountError::Refused(mountpoint.to_owned())
        } else {
            io(source)
        }
    })?;
    Ok(Mount {
        mountpoint: mountpoint.to_owned(),
        _session: session,
    })
}

/// Leave `mountpoint` as an empty directory that is not a mount point.
fn prepare_mountpoint(mountpoint: &Path) -> Result<(), MountError> {
    let io = |source| MountError::Io {
        path: mountpoint.to_owned(),
        source,
    };
    // ENOTCONN: a previous Skydock died without unmounting. The kernel
    // keeps the dead mount until someone detaches it.
    const ENOTCONN: i32 = 107;
    if let Err(error) = std::fs::metadata(mountpoint)
        && error.raw_os_error() == Some(ENOTCONN)
    {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(mountpoint)
            .status();
    }

    std::fs::create_dir_all(mountpoint).map_err(io)?;
    let here = std::fs::metadata(mountpoint).map_err(io)?;
    if let Some(parent) = mountpoint.parent()
        && let Ok(parent) = std::fs::metadata(parent)
        && parent.dev() != here.dev()
    {
        return Err(MountError::AlreadyMounted(mountpoint.to_owned()));
    }
    if std::fs::read_dir(mountpoint).map_err(io)?.next().is_some() {
        return Err(MountError::NotEmpty(mountpoint.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod mountpoint_tests {
    use super::*;

    #[test]
    fn mountpoint_is_created_and_must_be_empty() {
        let dir = std::env::temp_dir().join(format!("skydock-mnt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mountpoint = dir.join("Google Drive");

        prepare_mountpoint(&mountpoint).unwrap();
        assert!(mountpoint.is_dir());

        std::fs::write(mountpoint.join("file"), b"x").unwrap();
        assert!(matches!(
            prepare_mountpoint(&mountpoint),
            Err(MountError::NotEmpty(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
