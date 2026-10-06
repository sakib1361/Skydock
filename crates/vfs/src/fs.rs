//! The FUSE filesystem: a read-only view of one account. Folder listings
//! and file sizes come from the state database, so browsing never touches
//! the network; content is fetched the first time a file is opened.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use skydock_core::Provider;
use skydock_state::{Item, Store};
use tokio::runtime::Handle;

use crate::cache::Cache;

/// How long the kernel may reuse names and attributes before asking again.
/// Short, so changes fetched from the provider show up promptly.
const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

pub(crate) struct SkydockFs {
    inner: Arc<Inner>,
}

struct Inner {
    account: String,
    store: Mutex<Store>,
    inodes: Mutex<Inodes>,
    provider: Arc<dyn Provider>,
    cache: Cache,
    runtime: Handle,
    open_files: Mutex<HashMap<u64, Arc<File>>>,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

/// Inode numbers handed to the kernel, tied to provider item IDs. Numbers
/// are assigned as items are first seen and stay fixed while mounted.
struct Inodes {
    id_by_ino: HashMap<u64, String>,
    ino_by_id: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new(root_id: String) -> Self {
        Self {
            ino_by_id: HashMap::from([(root_id.clone(), ROOT_INO)]),
            id_by_ino: HashMap::from([(ROOT_INO, root_id)]),
            next: ROOT_INO + 1,
        }
    }

    fn ino_for(&mut self, id: &str) -> u64 {
        if let Some(ino) = self.ino_by_id.get(id) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.ino_by_id.insert(id.to_owned(), ino);
        self.id_by_ino.insert(ino, id.to_owned());
        ino
    }
}

impl SkydockFs {
    pub(crate) fn new(
        account: String,
        store: Store,
        root: &Item,
        provider: Arc<dyn Provider>,
        cache: Cache,
        runtime: Handle,
        (uid, gid): (u32, u32),
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                account,
                store: Mutex::new(store),
                inodes: Mutex::new(Inodes::new(root.id.clone())),
                provider,
                cache,
                runtime,
                open_files: Mutex::new(HashMap::new()),
                next_handle: AtomicU64::new(1),
                uid,
                gid,
            }),
        }
    }
}

impl Inner {
    fn item(&self, ino: INodeNo) -> Result<Item, Errno> {
        let id = self
            .inodes
            .lock()
            .unwrap()
            .id_by_ino
            .get(&u64::from(ino))
            .cloned()
            .ok_or(Errno::ENOENT)?;
        self.store
            .lock()
            .unwrap()
            .item(&self.account, &id)
            .map_err(|_| Errno::EIO)?
            .ok_or(Errno::ENOENT)
    }

    fn children(&self, folder: &Item) -> Result<Vec<Item>, Errno> {
        self.store
            .lock()
            .unwrap()
            .children(&self.account, &folder.id)
            .map_err(|_| Errno::EIO)
    }

    fn child(&self, folder: &Item, name: &str) -> Result<Option<Item>, Errno> {
        let exact = self
            .store
            .lock()
            .unwrap()
            .child(&self.account, &folder.id, name)
            .map_err(|_| Errno::EIO)?;
        if exact.is_some() {
            return Ok(exact);
        }
        // The name may be the local spelling of one that cannot exist on
        // Linux as written.
        Ok(self
            .children(folder)?
            .into_iter()
            .find(|item| local_name(&item.name) == name))
    }

    fn attr(&self, item: &Item) -> FileAttr {
        let ino = self.inodes.lock().unwrap().ino_for(&item.id);
        let size = if item.is_folder {
            0
        } else {
            item.size.unwrap_or(0)
        };
        let modified = item
            .modified
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino: INodeNo(ino),
            size,
            // Disk usage is real: nothing until the content has been fetched.
            blocks: if !item.is_folder && self.cache.is_cached(item) {
                size.div_ceil(512)
            } else {
                0
            },
            atime: modified,
            mtime: modified,
            ctime: modified,
            crtime: modified,
            kind: if item.is_folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if item.is_folder { 0o555 } else { 0o444 },
            nlink: if item.is_folder { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn register(&self, file: File) -> FileHandle {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.open_files
            .lock()
            .unwrap()
            .insert(handle, Arc::new(file));
        FileHandle(handle)
    }
}

impl Filesystem for SkydockFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let inner = &self.inner;
        let found = inner
            .item(parent)
            .and_then(|folder| inner.child(&folder, name.to_str().ok_or(Errno::ENOENT)?));
        match found {
            Ok(Some(item)) => reply.entry(&TTL, &inner.attr(&item), Generation(0)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.inner.item(ino) {
            Ok(item) => reply.attr(&TTL, &self.inner.attr(&item)),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = &self.inner;
        let children = match inner.item(ino).and_then(|folder| inner.children(&folder)) {
            Ok(children) => children,
            Err(errno) => return reply.error(errno),
        };
        let mut entries = vec![
            (u64::from(ino), FileType::Directory, ".".to_owned()),
            (u64::from(ino), FileType::Directory, "..".to_owned()),
        ];
        let mut inodes = inner.inodes.lock().unwrap();
        entries.extend(children.iter().map(|item| {
            let kind = if item.is_folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            (inodes.ino_for(&item.id), kind, local_name(&item.name))
        }));
        drop(inodes);

        for (index, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (index + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let inner = &self.inner;
        let item = match inner.item(ino) {
            Ok(item) if item.is_folder => return reply.error(Errno::EISDIR),
            // Provider-native documents: listed, but there is nothing to read.
            Ok(item) if item.size.is_none() && item.hash.is_none() => {
                return reply.error(Errno::ENOTSUP);
            }
            Ok(item) => item,
            Err(errno) => return reply.error(errno),
        };

        if inner.cache.is_cached(&item) {
            return match File::open(inner.cache.path_for(&item)) {
                Ok(file) => reply.opened(inner.register(file), FopenFlags::FOPEN_KEEP_CACHE),
                Err(error) => reply.error(Errno::from(error)),
            };
        }

        // Indexers and thumbnailers walk every file; letting them trigger
        // downloads would pull the whole drive down.
        if is_background_reader(req.pid()) {
            return reply.error(Errno::EACCES);
        }

        // Fetch off the filesystem thread so other files stay usable while
        // this one downloads; the opener waits for the reply.
        let inner = Arc::clone(inner);
        self.inner.runtime.spawn(async move {
            let opened = match inner.cache.hydrate(&*inner.provider, &item).await {
                Ok(path) => File::open(path).map_err(Errno::from),
                Err(error) => {
                    eprintln!("skydock: cannot fetch {}: {error}", item.name);
                    Err(Errno::EIO)
                }
            };
            match opened {
                Ok(file) => reply.opened(inner.register(file), FopenFlags::FOPEN_KEEP_CACHE),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = self
            .inner
            .open_files
            .lock()
            .unwrap()
            .get(&u64::from(fh))
            .cloned();
        let Some(file) = file else {
            return reply.error(Errno::EBADF);
        };
        let mut buffer = vec![0; size as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read_at(&mut buffer[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return reply.error(Errno::from(error)),
            }
        }
        reply.data(&buffer[..filled]);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.inner.open_files.lock().unwrap().remove(&u64::from(fh));
        reply.ok();
    }
}

/// A provider name as it appears locally. Providers allow names Linux
/// cannot hold (Google Drive permits `/`), so those are respelled.
pub(crate) fn local_name(name: &str) -> String {
    match name {
        "" | "." | ".." => "_".to_owned(),
        _ => name.replace(['/', '\0'], "_"),
    }
}

/// Whether the process opening a file is a known indexer or thumbnailer.
fn is_background_reader(pid: u32) -> bool {
    let read = |file: &str| std::fs::read(format!("/proc/{pid}/{file}")).unwrap_or_default();
    is_background_command(
        String::from_utf8_lossy(&read("comm")).trim(),
        &String::from_utf8_lossy(&read("cmdline")).replace('\0', " "),
    )
}

fn is_background_command(comm: &str, cmdline: &str) -> bool {
    const INDEXERS: [&str; 5] = [
        "baloo",
        "tracker-",
        "localsearch",
        "tumblerd",
        "recollindex",
    ];
    INDEXERS.iter().any(|prefix| comm.starts_with(prefix)) || cmdline.contains("thumbnail")
}

/// `2026-01-31T12:34:56Z` or with fractional seconds. Providers always
/// report UTC; anything else yields `None`.
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.split('.').next()?;
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return None;
    }

    // Days since 1970-01-01 in the proleptic Gregorian calendar, counting
    // years from March so the leap day falls at the end.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_to_unix_seconds() {
        let seconds = |text| {
            parse_rfc3339(text)
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(seconds("1970-01-01T00:00:00Z"), 0);
        assert_eq!(seconds("2000-03-01T00:00:00Z"), 951_868_800);
        assert_eq!(seconds("2024-02-29T23:59:59Z"), 1_709_251_199);
        assert_eq!(seconds("2026-10-06T05:47:25.123Z"), 1_791_265_645);
    }

    #[test]
    fn unusable_timestamps_are_rejected() {
        for text in [
            "",
            "2026-10-06",
            "2026-10-06T05:47:25+06:00",
            "2026-13-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }

    #[test]
    fn names_linux_cannot_hold_are_respelled() {
        assert_eq!(local_name("report.pdf"), "report.pdf");
        assert_eq!(local_name("a/b"), "a_b");
        assert_eq!(local_name(".."), "_");
    }

    #[test]
    fn indexers_and_thumbnailers_are_recognised() {
        assert!(is_background_command("baloo_file", "/usr/bin/baloo_file"));
        assert!(is_background_command(
            "kioworker",
            "/usr/lib/kf6/kioworker /usr/lib/qt6/plugins/kf6/kio/thumbnail.so thumbnail"
        ));
        assert!(!is_background_command("dolphin", "/usr/bin/dolphin"));
        assert!(!is_background_command("cat", "cat file.txt"));
    }

    #[test]
    fn inode_numbers_are_stable_and_root_is_one() {
        let mut inodes = Inodes::new("root-id".to_owned());
        assert_eq!(inodes.ino_for("root-id"), ROOT_INO);
        let a = inodes.ino_for("a");
        assert_eq!(inodes.ino_for("b"), a + 1);
        assert_eq!(inodes.ino_for("a"), a);
        assert_eq!(inodes.id_by_ino[&a], "a");
    }
}
