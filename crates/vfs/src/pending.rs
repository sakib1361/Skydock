//! Content changed on this device and not uploaded yet.
//!
//! Kept apart from the cache: cached files can be fetched again at any
//! time, these exist nowhere else until their upload finishes.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use skydock_core::hash::{HashKind, Hasher};

use crate::cache::item_part;

pub(crate) struct Pending {
    dir: PathBuf,
}

impl Pending {
    pub(crate) fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the changed content of an item is kept.
    pub(crate) fn path_for(&self, item_id: &str) -> PathBuf {
        self.dir.join(item_part(item_id))
    }

    /// Remove files that belong to none of `item_ids`: copies left half
    /// written, and content whose change was since uploaded or discarded.
    pub(crate) fn retain<'a>(&self, item_ids: impl Iterator<Item = &'a str>) {
        let keep: Vec<PathBuf> = item_ids.map(|id| self.path_for(id)).collect();
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if !keep.contains(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Write everything `source` holds to a new file at `dest`. Used where the
/// source's own name may be gone or on another filesystem.
pub(crate) fn copy_content(source: &File, dest: &Path) -> std::io::Result<()> {
    let partial = dest.with_extension("copying");
    let copied = (|| {
        let target = File::create(&partial)?;
        let mut buffer = vec![0; 1 << 20];
        let mut offset = 0;
        loop {
            match source.read_at(&mut buffer, offset)? {
                0 => break,
                read => {
                    target.write_all_at(&buffer[..read], offset)?;
                    offset += read as u64;
                }
            }
        }
        std::fs::rename(&partial, dest)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    copied
}

/// Give the file at `from` the name `to`, copying if they are on different
/// filesystems.
pub(crate) fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_content(&File::open(from)?, to)?;
    std::fs::remove_file(from)
}

/// Size of a file and its hash in the provider's form.
pub(crate) fn hash_file(path: &Path, kind: HashKind) -> std::io::Result<(u64, String)> {
    let file = File::open(path)?;
    let mut hasher = Hasher::new(kind);
    let mut buffer = vec![0; 1 << 20];
    let mut offset = 0;
    loop {
        match file.read_at(&mut buffer, offset)? {
            0 => return Ok((offset, hasher.finish())),
            read => {
                hasher.update(&buffer[..read]);
                offset += read as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(test: &str) -> Pending {
        let dir =
            std::env::temp_dir().join(format!("skydock-pending-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Pending::new(&dir).unwrap()
    }

    #[test]
    fn only_content_of_listed_items_is_kept() {
        let pending = pending("retain");
        std::fs::write(pending.path_for("a"), b"a").unwrap();
        std::fs::write(pending.path_for("b"), b"b").unwrap();
        std::fs::write(pending.path_for("a").with_extension("copying"), b"x").unwrap();

        pending.retain(["a"].into_iter());
        let left: Vec<_> = std::fs::read_dir(pending.dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(left, [pending.path_for("a")]);
    }

    #[test]
    fn content_is_copied_moved_and_hashed() {
        let pending = pending("copy");
        let (a, b, c) = (
            pending.path_for("a"),
            pending.path_for("b"),
            pending.path_for("c"),
        );
        std::fs::write(&a, b"abc").unwrap();

        copy_content(&File::open(&a).unwrap(), &b).unwrap();
        move_file(&b, &c).unwrap();
        assert!(a.exists() && !b.exists());
        assert_eq!(std::fs::read(&c).unwrap(), b"abc");
        assert_eq!(
            hash_file(&c, HashKind::Md5).unwrap(),
            (3, "900150983cd24fb0d6963f7d28e17f72".to_owned())
        );
    }
}
