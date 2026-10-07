//! The FUSE filesystem: one account as a folder. Listings and sizes come
//! from the state database, so browsing never touches the network; content
//! is fetched the first time a file is opened. Changed and new files are
//! kept on this device and uploaded once they are closed, while folders,
//! renames and deletions are carried out at the provider straight away.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow, WriteFlags,
};
use nix::libc;
use skydock_core::{Provider, RemoteItem};
use skydock_state::{Item, Store, is_local_id, new_local_id};
use tokio::runtime::Handle;

use crate::cache::Cache;
use crate::names::{is_transient, is_trash_folder, local_name};
use crate::pending::{Pending, copy_content};

/// How long the kernel may reuse names and attributes before asking again.
/// Short, so changes fetched from the provider show up promptly.
const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

/// How long a changed file is left alone after it is closed before it is
/// uploaded. Applications often save in several steps (write a temporary
/// file, then rename it over the document); this lets them finish.
pub(crate) const UPLOAD_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};

pub(crate) struct SkydockFs {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) account: String,
    pub(crate) store: Mutex<Store>,
    pub(crate) inodes: Mutex<Inodes>,
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) cache: Cache,
    pub(crate) pending: Pending,
    pub(crate) runtime: Handle,
    /// Lock order: `files` before `inodes` before `store`.
    pub(crate) files: Mutex<Files>,
    /// One lock per inode with remote work in progress (an upload, a rename,
    /// a deletion), so those never overlap for the same item.
    busy: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    /// Set on unmount; waiting uploads then give up.
    pub(crate) closed: AtomicBool,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

/// Inode numbers handed to the kernel, tied to provider item IDs. Numbers
/// are assigned as items are first seen and stay fixed while mounted.
pub(crate) struct Inodes {
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

    /// Make the inode that stood for `from` stand for `to`: a file got its
    /// provider ID on first upload, or took the place of another file. An
    /// inode `to` had before stands for nothing afterwards.
    pub(crate) fn rebind(&mut self, from: &str, to: &str) {
        let Some(ino) = self.ino_by_id.remove(from) else {
            return;
        };
        if let Some(replaced) = self.ino_by_id.insert(to.to_owned(), ino) {
            self.id_by_ino.remove(&replaced);
        }
        self.id_by_ino.insert(ino, to.to_owned());
    }
}

/// Open files and the files with changes that are not uploaded yet.
#[derive(Default)]
pub(crate) struct Files {
    handles: HashMap<u64, OpenFile>,
    /// Inode of each changed file, with a count of the writes made to it. An
    /// upload compares the count before and after to learn whether what it
    /// sent is still what the file holds.
    pub(crate) dirty: HashMap<u64, u64>,
    /// Number of the latest upload planned per inode; earlier plans lapse.
    planned: HashMap<u64, u64>,
}

struct OpenFile {
    file: Arc<File>,
    ino: u64,
    writable: bool,
    /// For a file opened without local changes: the item as it was then,
    /// which names the cached content this handle reads and what a first
    /// write is based on.
    origin: Option<Item>,
}

impl Files {
    pub(crate) fn has_writers(&self, ino: u64) -> bool {
        self.handles
            .values()
            .any(|open| open.ino == ino && open.writable)
    }

    /// Point every handle of `ino` at the file now at `path`, after its
    /// content moved between the cache and the pending area.
    pub(crate) fn reopen(&mut self, ino: u64, path: &Path, origin: Option<&Item>) {
        for open in self.handles.values_mut().filter(|open| open.ino == ino) {
            match open_with(path, open.writable) {
                Ok(file) => {
                    open.file = Arc::new(file);
                    open.origin = origin.cloned();
                }
                Err(error) => eprintln!("skydock: cannot reopen {}: {error}", path.display()),
            }
        }
    }

    fn touch(&mut self, ino: u64) {
        *self.dirty.entry(ino).or_default() += 1;
    }
}

/// What a first change to a clean file starts from.
enum Content<'a> {
    Empty,
    /// The cached content this open file reads.
    Of(&'a File),
}

enum Opened {
    Handle(FileHandle),
    /// The content has to be downloaded first.
    Fetch(Item),
}

impl SkydockFs {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        account: String,
        store: Store,
        root: &Item,
        provider: Arc<dyn Provider>,
        cache: Cache,
        pending: Pending,
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
                pending,
                runtime,
                files: Mutex::default(),
                busy: Mutex::default(),
                closed: AtomicBool::new(false),
                next_handle: AtomicU64::new(1),
                uid,
                gid,
            }),
        }
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl Inner {
    /// Run a state database action, reporting failure as an I/O error.
    pub(crate) fn store<T>(
        &self,
        action: impl FnOnce(&mut Store, &str) -> skydock_state::Result<T>,
    ) -> Result<T, Errno> {
        action(&mut self.store.lock().unwrap(), &self.account).map_err(|error| {
            eprintln!("skydock: state database: {error}");
            Errno::EIO
        })
    }

    pub(crate) fn ino_of(&self, id: &str) -> u64 {
        self.inodes.lock().unwrap().ino_for(id)
    }

    pub(crate) fn item(&self, ino: INodeNo) -> Result<Item, Errno> {
        let id = self
            .inodes
            .lock()
            .unwrap()
            .id_by_ino
            .get(&u64::from(ino))
            .cloned()
            .ok_or(Errno::ENOENT)?;
        self.stored(&id)
    }

    fn stored(&self, id: &str) -> Result<Item, Errno> {
        self.store(|store, account| store.item(account, id))?
            .ok_or(Errno::ENOENT)
    }

    fn folder(&self, ino: INodeNo) -> Result<Item, Errno> {
        match self.item(ino)? {
            item if item.is_folder => Ok(item),
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn children(&self, folder: &Item) -> Result<Vec<Item>, Errno> {
        self.store(|store, account| store.children(account, &folder.id))
    }

    fn child(&self, folder: &Item, name: &str) -> Result<Option<Item>, Errno> {
        let exact = self.store(|store, account| store.child(account, &folder.id, name))?;
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

    /// What stands in the way of creating `name` in a folder: the entry of
    /// that name or, where the provider does not tell letter case apart,
    /// one that differs only in case.
    fn occupant(&self, folder: &Item, name: &str) -> Result<Option<Item>, Errno> {
        if let Some(item) = self.child(folder, name)? {
            return Ok(Some(item));
        }
        if self.provider.names_are_case_sensitive() {
            return Ok(None);
        }
        let wanted = name.to_lowercase();
        Ok(self
            .children(folder)?
            .into_iter()
            .find(|item| item.name.to_lowercase() == wanted))
    }

    /// Check a name an application wants to give a new or renamed entry.
    fn usable_name(&self, folder: &Item, name: &OsStr) -> Result<String, Errno> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        let at_top = self.ino_of(&folder.id) == ROOT_INO;
        if at_top && is_trash_folder(name) {
            return Err(Errno::EPERM);
        }
        // Scratch files stay on this device, so the provider's rules do not
        // apply to them.
        if !is_transient(name) && !self.provider.accepts_name(name) {
            return Err(Errno::EINVAL);
        }
        Ok(name.to_owned())
    }

    fn is_dirty(&self, ino: u64) -> bool {
        self.files.lock().unwrap().dirty.contains_key(&ino)
    }

    fn attr(&self, item: &Item) -> FileAttr {
        let ino = self.ino_of(&item.id);
        // A file with local changes is what this device holds, whatever the
        // provider last reported.
        let local = match self.is_dirty(ino) {
            true => std::fs::metadata(self.pending.path_for(&item.id)).ok(),
            false => None,
        };
        let remote_time = || {
            item.modified
                .as_deref()
                .and_then(parse_rfc3339)
                .unwrap_or(UNIX_EPOCH)
        };
        let (size, modified, on_disk) = match &local {
            Some(local) => (
                local.len(),
                local.modified().unwrap_or_else(|_| remote_time()),
                true,
            ),
            None if item.is_folder => (0, remote_time(), false),
            None => (
                item.size.unwrap_or(0),
                remote_time(),
                self.cache.is_cached(item),
            ),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            // Disk usage is real: nothing until the content has been fetched.
            blocks: if on_disk { size.div_ceil(512) } else { 0 },
            atime: modified,
            mtime: modified,
            ctime: modified,
            crtime: modified,
            kind: if item.is_folder {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: match item {
                item if item.is_folder => 0o755,
                item if is_native(item) => 0o444,
                _ => 0o644,
            },
            nlink: if item.is_folder { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn register(&self, files: &mut Files, open: OpenFile) -> FileHandle {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        files.handles.insert(handle, open);
        FileHandle(handle)
    }

    /// Wait until no other remote work is under way for `ino`.
    pub(crate) async fn busy(&self, ino: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.busy.lock().unwrap().entry(ino).or_default().clone();
        lock.lock_owned().await
    }

    /// Turn a provider failure into what the application is told, and say
    /// on the terminal what actually happened.
    fn remote_error(&self, action: &str, name: &str, error: skydock_core::Error) -> Errno {
        eprintln!("skydock: cannot {action} {name}: {error}");
        match error.status() {
            Some(403) => Errno::EACCES,
            Some(404) => Errno::ENOENT,
            Some(409) => Errno::EEXIST,
            Some(507) => Errno::ENOSPC,
            _ => Errno::EIO,
        }
    }

    /// Start a local change to a file that has none: its content moves out
    /// of the cache into the pending area, where it stays until uploaded.
    fn begin_change(
        &self,
        files: &mut Files,
        ino: u64,
        base: &Item,
        content: Content,
    ) -> std::io::Result<()> {
        let dest = self.pending.path_for(&base.id);
        match content {
            Content::Empty => drop(File::create(&dest)?),
            Content::Of(file) => {
                let cached = self.cache.path_for(base);
                // The cached file may be gone ("Free up space") or on
                // another filesystem; the open file still has the content.
                if std::fs::rename(&cached, &dest).is_err() {
                    copy_content(file, &dest)?;
                    let _ = std::fs::remove_file(&cached);
                }
            }
        }
        self.store
            .lock()
            .unwrap()
            .mark_dirty(&self.account, &base.id, Some(base))
            .map_err(std::io::Error::other)?;
        files.dirty.insert(ino, 0);
        files.reopen(ino, &dest, None);
        Ok(())
    }

    fn open_file(&self, ino: u64, write: bool, truncate: bool) -> Result<Opened, Errno> {
        let mut files = self.files.lock().unwrap();
        let item = self.item(INodeNo(ino))?;
        if item.is_folder {
            return Err(Errno::EISDIR);
        }
        if !files.dirty.contains_key(&ino) {
            if is_native(&item) {
                // Listed, but there is nothing to read or replace.
                return Err(if write { Errno::EACCES } else { Errno::ENOTSUP });
            }
            if truncate {
                // The old content is about to go; no need to fetch it.
                self.begin_change(&mut files, ino, &item, Content::Empty)
                    .map_err(io_errno)?;
            } else {
                return match open_with(&self.cache.path_for(&item), write) {
                    Ok(file) => Ok(Opened::Handle(self.register(
                        &mut files,
                        OpenFile {
                            file: Arc::new(file),
                            ino,
                            writable: write,
                            origin: Some(item),
                        },
                    ))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Ok(Opened::Fetch(item))
                    }
                    Err(error) => Err(io_errno(error)),
                };
            }
        }
        let file = open_with(&self.pending.path_for(&item.id), write).map_err(io_errno)?;
        if truncate {
            file.set_len(0).map_err(io_errno)?;
            files.touch(ino);
        }
        Ok(Opened::Handle(self.register(
            &mut files,
            OpenFile {
                file: Arc::new(file),
                ino,
                writable: write,
                origin: None,
            },
        )))
    }

    fn create_file(&self, parent: INodeNo, name: &OsStr) -> Result<(FileAttr, FileHandle), Errno> {
        let folder = self.folder(parent)?;
        let name = self.usable_name(&folder, name)?;
        if self.occupant(&folder, &name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let created = RemoteItem {
            id: new_local_id(),
            parent_id: Some(folder.id),
            name,
            is_folder: false,
            size: Some(0),
            version: None,
            hash: None,
            modified: Some(format_rfc3339(SystemTime::now())),
        };
        let mut files = self.files.lock().unwrap();
        let path = self.pending.path_for(&created.id);
        File::create(&path).map_err(io_errno)?;
        let file = open_with(&path, true).map_err(io_errno)?;
        self.store(|store, account| {
            store.put(account, &created)?;
            store.mark_dirty(account, &created.id, None)
        })?;
        let ino = self.ino_of(&created.id);
        files.dirty.insert(ino, 0);
        let handle = self.register(
            &mut files,
            OpenFile {
                file: Arc::new(file),
                ino,
                writable: true,
                origin: None,
            },
        );
        drop(files);
        Ok((self.attr(&self.stored(&created.id)?), handle))
    }

    fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let mut files = self.files.lock().unwrap();
        let (ino, file, origin) = match files.handles.get(&fh) {
            Some(open) if open.writable => (open.ino, Arc::clone(&open.file), open.origin.clone()),
            _ => return Err(Errno::EBADF),
        };
        let file = if files.dirty.contains_key(&ino) {
            file
        } else {
            // The handle may outlive the item it was opened on.
            let base = origin.ok_or(Errno::EIO)?;
            self.item(INodeNo(ino))?;
            self.begin_change(&mut files, ino, &base, Content::Of(&file))
                .map_err(io_errno)?;
            Arc::clone(&files.handles[&fh].file)
        };
        file.write_all_at(data, offset).map_err(io_errno)?;
        files.touch(ino);
        Ok(())
    }

    /// Set a file's length, fetching its content first if it has to be kept.
    async fn resize(self: &Arc<Self>, ino: u64, size: u64) -> Result<(), Errno> {
        let item = self.item(INodeNo(ino))?;
        if item.is_folder {
            return Err(Errno::EISDIR);
        }
        let needs_content = size > 0 && !is_native(&item) && !self.is_dirty(ino);
        if needs_content && !self.cache.is_cached(&item) {
            self.cache
                .hydrate(&*self.provider, &item)
                .await
                .map_err(|error| {
                    eprintln!("skydock: cannot fetch {}: {error}", item.name);
                    Errno::EIO
                })?;
        }

        let mut files = self.files.lock().unwrap();
        let item = self.item(INodeNo(ino))?;
        if !files.dirty.contains_key(&ino) {
            if is_native(&item) {
                return Err(Errno::EACCES);
            }
            let started = match size {
                0 => self.begin_change(&mut files, ino, &item, Content::Empty),
                _ => File::open(self.cache.path_for(&item)).and_then(|cached| {
                    self.begin_change(&mut files, ino, &item, Content::Of(&cached))
                }),
            };
            started.map_err(io_errno)?;
        }
        open_with(&self.pending.path_for(&item.id), true)
            .and_then(|file| file.set_len(size))
            .map_err(io_errno)?;
        files.touch(ino);
        let has_writers = files.has_writers(ino);
        drop(files);
        // Nobody will close the file, so nothing else would start the upload.
        if !has_writers {
            self.plan_upload(ino, UPLOAD_DELAY, 0);
        }
        Ok(())
    }

    async fn make_folder(&self, parent: INodeNo, name: &OsStr) -> Result<FileAttr, Errno> {
        let folder = self.folder(parent)?;
        let name = self.usable_name(&folder, name)?;
        if self.occupant(&folder, &name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let created = self
            .provider
            .create_folder(&folder.id, &name)
            .await
            .map_err(|error| self.remote_error("create folder", &name, error))?;
        self.store(|store, account| store.put(account, &created))?;
        Ok(self.attr(&self.stored(&created.id)?))
    }

    /// Delete the entry `name`, which must be a folder or must not be one.
    async fn remove(
        self: &Arc<Self>,
        parent: INodeNo,
        name: &OsStr,
        folder_wanted: bool,
    ) -> Result<(), Errno> {
        let folder = self.folder(parent)?;
        let found = self
            .child(&folder, name.to_str().ok_or(Errno::ENOENT)?)?
            .ok_or(Errno::ENOENT)?;
        let ino = self.ino_of(&found.id);
        let _busy = self.busy(ino).await;
        // An upload that just finished may have given the file a new ID.
        let item = self.item(INodeNo(ino))?;
        match (folder_wanted, item.is_folder) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        if item.is_folder && !self.children(&item)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        self.discard(ino, &item).await
    }

    /// Delete an item here and, unless it never got there, at the provider.
    /// The caller holds the item's busy lock.
    async fn discard(&self, ino: u64, item: &Item) -> Result<(), Errno> {
        if !is_local_id(&item.id) {
            self.provider
                .delete(&item.id)
                .await
                .map_err(|error| self.remote_error("delete", &item.name, error))?;
        }
        let mut files = self.files.lock().unwrap();
        self.store(|store, account| store.remove(account, &item.id))?;
        files.dirty.remove(&ino);
        files.planned.remove(&ino);
        drop(files);
        // Whoever has the file open keeps reading what they opened.
        let _ = std::fs::remove_file(self.pending.path_for(&item.id));
        self.cache.remove_all_copies(&item.id);
        Ok(())
    }

    async fn rename(
        self: &Arc<Self>,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        replace: bool,
    ) -> Result<(), Errno> {
        let from = self.folder(parent)?;
        let to = self.folder(new_parent)?;
        let found = self
            .child(&from, name.to_str().ok_or(Errno::ENOENT)?)?
            .ok_or(Errno::ENOENT)?;
        let new_name = self.usable_name(&to, new_name)?;
        // Renaming a file to another spelling of its own name replaces nothing.
        let in_the_way = self
            .occupant(&to, &new_name)?
            .filter(|occupant| occupant.id != found.id);
        if in_the_way.is_some() && !replace {
            return Err(Errno::EEXIST);
        }

        // Always in the same order, so two renames cannot wait on each other.
        let ino = self.ino_of(&found.id);
        let other_ino = in_the_way.as_ref().map(|item| self.ino_of(&item.id));
        let mut order = vec![ino];
        order.extend(other_ino.filter(|other| *other != ino));
        order.sort_unstable();
        let mut held = Vec::new();
        for ino in order {
            held.push(self.busy(ino).await);
        }

        let source = self.item(INodeNo(ino))?;
        if let Some(other_ino) = other_ino {
            let target = self.item(INodeNo(other_ino))?;
            match (source.is_folder, target.is_folder) {
                (false, true) => return Err(Errno::EISDIR),
                (true, false) => return Err(Errno::ENOTDIR),
                (true, true) if !self.children(&target)?.is_empty() => {
                    return Err(Errno::ENOTEMPTY);
                }
                _ => {}
            }
            // An application saving through a temporary file: what it wrote
            // becomes the document's new content, so the document keeps its
            // identity and history at the provider.
            if is_local_id(&source.id) && !is_local_id(&target.id) && !is_native(&target) {
                return self.adopt(ino, &source, other_ino, &target);
            }
            self.discard(other_ino, &target).await?;
        }

        if is_local_id(&source.id) {
            // Never uploaded: only this device knows the file.
            let moved = RemoteItem {
                id: source.id.clone(),
                parent_id: Some(to.id),
                name: new_name,
                is_folder: false,
                size: source.size,
                version: None,
                hash: None,
                modified: source.modified.clone(),
            };
            self.store(|store, account| store.put(account, &moved))?;
            if !self.files.lock().unwrap().has_writers(ino) {
                self.plan_upload(ino, UPLOAD_DELAY, 0);
            }
            return Ok(());
        }
        let moved = self
            .provider
            .move_item(&source.id, &from.id, &to.id, &new_name)
            .await
            .map_err(|error| self.remote_error("rename", &source.name, error))?;
        self.store(|store, account| store.put(account, &moved))
    }

    /// Give `target` the content written to the never-uploaded `source`,
    /// which then ceases to exist as a file of its own.
    fn adopt(
        self: &Arc<Self>,
        ino: u64,
        source: &Item,
        target_ino: u64,
        target: &Item,
    ) -> Result<(), Errno> {
        let mut files = self.files.lock().unwrap();
        std::fs::rename(
            self.pending.path_for(&source.id),
            self.pending.path_for(&target.id),
        )
        .map_err(io_errno)?;
        self.store(|store, account| {
            // Kept if the document already had a change waiting.
            store.mark_dirty(account, &target.id, Some(target))?;
            store.remove(account, &source.id)
        })?;
        // The kernel now knows the document by the temporary file's inode.
        self.inodes.lock().unwrap().rebind(&source.id, &target.id);
        files.dirty.remove(&target_ino);
        files.planned.remove(&target_ino);
        files.touch(ino);
        let has_writers = files.has_writers(ino);
        drop(files);
        if !has_writers {
            self.plan_upload(ino, UPLOAD_DELAY, 0);
        }
        Ok(())
    }

    /// Upload `ino`'s changes after `delay`, unless planned again by then.
    pub(crate) fn plan_upload(self: &Arc<Self>, ino: u64, delay: Duration, attempt: u32) {
        let plan = {
            let mut files = self.files.lock().unwrap();
            let plan = files.planned.entry(ino).or_default();
            *plan += 1;
            *plan
        };
        let inner = Arc::clone(self);
        self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            let lapsed = |inner: &Inner| {
                inner.closed.load(Ordering::Relaxed)
                    || inner.files.lock().unwrap().planned.get(&ino) != Some(&plan)
            };
            if lapsed(&inner) {
                return;
            }
            let busy = inner.busy(ino).await;
            if lapsed(&inner) {
                return;
            }
            let outcome = inner.upload(ino).await;
            drop(busy);
            match outcome {
                Ok(crate::upload::Outcome::Settled) => {}
                Ok(crate::upload::Outcome::ChangedMeanwhile) => {
                    inner.plan_upload(ino, UPLOAD_DELAY, 0)
                }
                Err(error) => {
                    let delay = crate::upload::retry_delay(attempt);
                    eprintln!(
                        "skydock: upload failed, trying again in {} s: {error}",
                        delay.as_secs()
                    );
                    inner.plan_upload(ino, delay, attempt + 1);
                }
            }
        });
    }
}

impl Filesystem for SkydockFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Have a truncating open arrive as one request. Otherwise the open
        // comes first, on its own, and fetches content that the truncation
        // right behind it throws away. Older kernels just do it that way.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn destroy(&mut self) {
        self.inner.closed.store(true, Ordering::Relaxed);
    }

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

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Owner and permissions are fixed, and providers set the times
        // themselves. Such requests are accepted so that copying tools do
        // not report failures, but only the size is acted on.
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            let ino = u64::from(ino);
            let resized = match size {
                Some(size) => inner.resize(ino, size).await,
                None => Ok(()),
            };
            let attr = resized.and_then(|()| {
                let item = inner.item(INodeNo(ino))?;
                // Until the upload, show the time the application asked for.
                if let Some(mtime) = mtime
                    && inner.is_dirty(ino)
                    && let Ok(file) = open_with(&inner.pending.path_for(&item.id), true)
                {
                    let _ = file.set_modified(match mtime {
                        TimeOrNow::SpecificTime(time) => time,
                        TimeOrNow::Now => SystemTime::now(),
                    });
                }
                Ok(inner.attr(&item))
            });
            match attr {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let (inner, name) = (Arc::clone(&self.inner), name.to_owned());
        self.inner.runtime.spawn(async move {
            match inner.make_folder(parent, &name).await {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (inner, name) = (Arc::clone(&self.inner), name.to_owned());
        self.inner.runtime.spawn(async move {
            match inner.remove(parent, &name, false).await {
                Ok(()) => reply.ok(),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (inner, name) = (Arc::clone(&self.inner), name.to_owned());
        self.inner.runtime.spawn(async move {
            match inner.remove(parent, &name, true).await {
                Ok(()) => reply.ok(),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
            return reply.error(Errno::EINVAL);
        }
        let replace = !flags.contains(RenameFlags::RENAME_NOREPLACE);
        let (inner, name, newname) = (Arc::clone(&self.inner), name.to_owned(), newname.to_owned());
        self.inner.runtime.spawn(async move {
            match inner
                .rename(parent, &name, newparent, &newname, replace)
                .await
            {
                Ok(()) => reply.ok(),
                Err(errno) => reply.error(errno),
            }
        });
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

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let inner = &self.inner;
        let ino = u64::from(ino);
        let write = flags.0 & libc::O_ACCMODE != libc::O_RDONLY;
        let truncate = write && flags.0 & libc::O_TRUNC != 0;
        let item = match inner.open_file(ino, write, truncate) {
            Ok(Opened::Handle(handle)) => {
                return reply.opened(handle, FopenFlags::FOPEN_KEEP_CACHE);
            }
            Ok(Opened::Fetch(item)) => item,
            Err(errno) => return reply.error(errno),
        };

        // Indexers and thumbnailers walk every file; letting them trigger
        // downloads would pull the whole drive down.
        if is_background_reader(req.pid()) {
            return reply.error(Errno::EACCES);
        }

        // Fetch off the filesystem thread so other files stay usable while
        // this one downloads; the opener waits for the reply.
        let inner = Arc::clone(inner);
        self.inner.runtime.spawn(async move {
            if let Err(error) = inner.cache.hydrate(&*inner.provider, &item).await {
                eprintln!("skydock: cannot fetch {}: {error}", item.name);
                return reply.error(Errno::EIO);
            }
            match inner.open_file(ino, write, truncate) {
                Ok(Opened::Handle(handle)) => reply.opened(handle, FopenFlags::FOPEN_KEEP_CACHE),
                // It changed remotely in the moment since it was fetched.
                Ok(Opened::Fetch(_)) => reply.error(Errno::EIO),
                Err(errno) => reply.error(errno),
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.inner.create_file(parent, name) {
            Ok((attr, handle)) => {
                reply.created(&TTL, &attr, Generation(0), handle, FopenFlags::empty())
            }
            Err(errno) => reply.error(errno),
        }
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
            .files
            .lock()
            .unwrap()
            .handles
            .get(&u64::from(fh))
            .map(|open| Arc::clone(&open.file));
        let Some(file) = file else {
            return reply.error(Errno::EBADF);
        };
        let mut buffer = vec![0; size as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read_at(&mut buffer[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return reply.error(io_errno(error)),
            }
        }
        reply.data(&buffer[..filled]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.inner.write(u64::from(fh), offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Writes go straight to the local file; there is nothing buffered.
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let file = self
            .inner
            .files
            .lock()
            .unwrap()
            .handles
            .get(&u64::from(fh))
            .map(|open| Arc::clone(&open.file));
        match file.map(|file| file.sync_data()) {
            Some(Ok(())) => reply.ok(),
            Some(Err(error)) => reply.error(io_errno(error)),
            None => reply.error(Errno::EBADF),
        }
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
        let mut files = self.inner.files.lock().unwrap();
        let closed = files.handles.remove(&u64::from(fh));
        // The last writer is done: time to send what changed.
        let upload = closed
            .filter(|open| open.writable)
            .map(|open| open.ino)
            .filter(|ino| files.dirty.contains_key(ino) && !files.has_writers(*ino));
        drop(files);
        if let Some(ino) = upload {
            self.inner.plan_upload(ino, UPLOAD_DELAY, 0);
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Everything written lands on this device first, so the space here
        // is what limits a write. Without an answer file managers assume
        // the folder is full.
        match nix::sys::statvfs::statvfs(self.inner.pending.dir()) {
            Ok(disk) => reply.statfs(
                disk.blocks(),
                disk.blocks_free(),
                disk.blocks_available(),
                disk.files(),
                disk.files_free(),
                disk.block_size() as u32,
                255,
                disk.fragment_size() as u32,
            ),
            Err(errno) => reply.error(Errno::from_i32(errno as i32)),
        }
    }
}

/// Provider-native documents (Google Docs and the like): listed, but with
/// no bytes to read or replace.
pub(crate) fn is_native(item: &Item) -> bool {
    !item.is_folder && item.size.is_none() && item.hash.is_none() && !is_local_id(&item.id)
}

fn open_with(path: &Path, write: bool) -> std::io::Result<File> {
    File::options().read(true).write(write).open(path)
}

pub(crate) fn io_errno(error: std::io::Error) -> Errno {
    match error.raw_os_error() {
        Some(code) => Errno::from_i32(code),
        None => Errno::EIO,
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

/// The inverse of [`parse_rfc3339`], to the second.
fn format_rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));

    // Days to a civil date, again counting years from March.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
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
    fn timestamps_are_written_the_way_they_are_read() {
        for text in [
            "1970-01-01T00:00:00Z",
            "2000-02-29T12:00:00Z",
            "2024-12-31T23:59:59Z",
            "2026-10-07T16:41:02Z",
        ] {
            assert_eq!(format_rfc3339(parse_rfc3339(text).unwrap()), text);
        }
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

    #[test]
    fn an_inode_follows_its_file_to_a_new_id() {
        let mut inodes = Inodes::new("root-id".to_owned());
        let (temporary, document) = (inodes.ino_for("local-1"), inodes.ino_for("doc"));

        // First upload: the provider's ID replaces the local one.
        inodes.rebind("local-1", "remote-1");
        assert_eq!(inodes.ino_for("remote-1"), temporary);
        assert!(!inodes.ino_by_id.contains_key("local-1"));

        // Saved over a document: the document is now this inode.
        inodes.rebind("remote-1", "doc");
        assert_eq!(inodes.ino_for("doc"), temporary);
        assert!(!inodes.id_by_ino.contains_key(&document));
    }
}
