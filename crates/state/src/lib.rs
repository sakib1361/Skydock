//! Local SQLite mirror of remote metadata, for every provider and account.
//!
//! Rows are keyed by `(account, item_id)` and link to their parent by ID. The
//! account key is chosen by the caller and must be unique across providers.
//! Paths are never stored: delta does not report the descendants of a renamed
//! or moved folder, so a stored path would silently go stale.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use skydock_core::{Change, ChangeSet, RemoteItem, latest_per_item};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("state database has schema version {found}, this build supports up to {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
}

/// One entry per schema version; a database at version N has had the first
/// N applied. Only ever append.
const MIGRATIONS: [&str; 2] = [SCHEMA, KNOWN_ACCOUNTS];

const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA: &str = "
CREATE TABLE accounts (
    key    TEXT PRIMARY KEY,
    cursor TEXT
) WITHOUT ROWID;

CREATE TABLE items (
    account   TEXT NOT NULL,
    id        TEXT NOT NULL,
    parent_id TEXT,             -- NULL only for the drive root
    name      TEXT NOT NULL,
    is_folder INTEGER NOT NULL,
    size      INTEGER,
    version   TEXT,
    hash      TEXT,
    modified  TEXT,
    PRIMARY KEY (account, id)
) WITHOUT ROWID;

CREATE INDEX items_by_parent ON items (account, parent_id, name);
";

/// Which account each provider is signed in to, so work on the mirror does
/// not need the network to find out.
const KNOWN_ACCOUNTS: &str = "
CREATE TABLE providers (
    id         TEXT PRIMARY KEY,
    account_id TEXT NOT NULL
) WITHOUT ROWID;
";

/// What applying one change set altered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub upserted: usize,
    pub deleted: usize,
    /// Folders the service reported deleted that still have children here.
    /// They are kept so the children stay reachable.
    pub folders_kept: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub folders: u64,
    pub files: u64,
    pub bytes: u64,
}

/// One stored item, as needed to list a folder or fetch a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    pub version: Option<String>,
    pub hash: Option<String>,
    pub modified: Option<String>,
}

const ITEM_COLUMNS: &str = "id, name, is_folder, size, version, hash, modified";

fn read_item(row: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        name: row.get(1)?,
        is_folder: row.get(2)?,
        size: row.get::<_, Option<i64>>(3)?.map(|size| size as u64),
        version: row.get(4)?,
        hash: row.get(5)?,
        modified: row.get(6)?,
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if found < SCHEMA_VERSION {
            let tx = conn.transaction()?;
            for migration in &MIGRATIONS[found as usize..] {
                tx.execute_batch(migration)?;
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// The account ID last recorded for a provider.
    pub fn known_account(&self, provider: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT account_id FROM providers WHERE id = ?1",
                [provider],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_known_account(&self, provider: &str, account_id: Option<&str>) -> Result<()> {
        match account_id {
            Some(account_id) => self.conn.execute(
                "INSERT INTO providers (id, account_id) VALUES (?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id",
                [provider, account_id],
            )?,
            None => self
                .conn
                .execute("DELETE FROM providers WHERE id = ?1", [provider])?,
        };
        Ok(())
    }

    /// The position to resume from, if this account has been enumerated.
    pub fn cursor(&self, account: &str) -> Result<Option<String>> {
        let cursor = self
            .conn
            .query_row(
                "SELECT cursor FROM accounts WHERE key = ?1",
                [account],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(cursor.flatten())
    }

    /// Apply one complete change set and record its cursor, atomically:
    /// either every change and the new cursor are stored, or nothing is.
    pub fn apply(&mut self, account: &str, set: ChangeSet) -> Result<Applied> {
        let tx = self.conn.transaction()?;
        let mut applied = Applied::default();
        let mut deleted_folders = Vec::new();

        if set.full {
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS seen (id TEXT PRIMARY KEY) WITHOUT ROWID;
                 DELETE FROM seen;",
            )?;
        }

        for change in latest_per_item(set.changes) {
            match change {
                // A folder may only go once it is empty, which is not known
                // until the whole set is in.
                Change::Delete { id } => match is_folder(&tx, account, &id)? {
                    None => {}
                    Some(true) => deleted_folders.push(id),
                    Some(false) => applied.deleted += delete(&tx, account, &id)?,
                },
                Change::Upsert(item) => {
                    upsert(&tx, account, &item)?;
                    applied.upserted += 1;
                    if set.full {
                        tx.execute("INSERT OR IGNORE INTO seen (id) VALUES (?1)", [&item.id])?;
                    }
                }
            }
        }

        // A full enumeration lists everything, so whatever it did not
        // mention no longer exists remotely.
        if set.full {
            applied.deleted += tx.execute(
                "DELETE FROM items WHERE account = ?1 AND id NOT IN (SELECT id FROM seen)",
                [account],
            )?;
        }

        // Repeat so that nested deleted folders clear from the inside out.
        loop {
            let mut remaining = Vec::new();
            for id in &deleted_folders {
                if has_children(&tx, account, id)? {
                    remaining.push(id.clone());
                } else {
                    applied.deleted += delete(&tx, account, id)?;
                }
            }
            let progressed = remaining.len() < deleted_folders.len();
            deleted_folders = remaining;
            if !progressed {
                break;
            }
        }
        applied.folders_kept = deleted_folders.len();

        tx.execute(
            "INSERT INTO accounts (key, cursor) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET cursor = excluded.cursor",
            params![account, set.cursor],
        )?;
        tx.commit()?;
        Ok(applied)
    }

    pub fn totals(&self, account: &str) -> Result<Totals> {
        let totals = self.conn.query_row(
            "SELECT COALESCE(SUM(is_folder AND parent_id IS NOT NULL), 0),
                    COALESCE(SUM(NOT is_folder), 0),
                    COALESCE(SUM(CASE WHEN is_folder THEN 0 ELSE size END), 0)
             FROM items WHERE account = ?1",
            [account],
            |row| {
                Ok(Totals {
                    folders: row.get::<_, i64>(0)? as u64,
                    files: row.get::<_, i64>(1)? as u64,
                    bytes: row.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        Ok(totals)
    }

    /// Contents of a folder, folders first, then by name.
    pub fn children(&self, account: &str, folder_id: &str) -> Result<Vec<Item>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE account = ?1 AND parent_id = ?2
             ORDER BY is_folder DESC, name COLLATE NOCASE"
        ))?;
        let items = statement
            .query_map([account, folder_id], read_item)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(items)
    }

    pub fn root(&self, account: &str) -> Result<Option<Item>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM items WHERE account = ?1 AND parent_id IS NULL"
                ),
                [account],
                read_item,
            )
            .optional()?)
    }

    pub fn item(&self, account: &str, id: &str) -> Result<Option<Item>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE account = ?1 AND id = ?2"
        ))?;
        Ok(statement.query_row([account, id], read_item).optional()?)
    }

    /// The entry called exactly `name` in a folder.
    pub fn child(&self, account: &str, folder_id: &str, name: &str) -> Result<Option<Item>> {
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE account = ?1 AND parent_id = ?2 AND name = ?3"
        ))?;
        Ok(statement
            .query_row([account, folder_id, name], read_item)
            .optional()?)
    }

    /// The item at `path` (as produced by [`Self::path`]; `/` or the empty
    /// string is the root). Names must match exactly.
    pub fn resolve(&self, account: &str, path: &str) -> Result<Option<Item>> {
        let Some(mut current) = self.root(account)? else {
            return Ok(None);
        };
        for name in path.split('/').filter(|part| !part.is_empty()) {
            match self.child(account, &current.id, name)? {
                Some(child) => current = child,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// Path of an item relative to the drive root, with a leading slash; the
    /// root itself is `/`. `None` if the item is unknown or its ancestry does
    /// not reach the root.
    pub fn path(&self, account: &str, item_id: &str) -> Result<Option<String>> {
        let mut statement = self.conn.prepare_cached(
            "WITH RECURSIVE chain (parent_id, name, depth) AS (
                 SELECT parent_id, name, 0 FROM items WHERE account = ?1 AND id = ?2
                 UNION ALL
                 SELECT i.parent_id, i.name, c.depth + 1
                 FROM items i JOIN chain c ON i.id = c.parent_id
                 WHERE i.account = ?1 AND c.depth < 1024
             )
             SELECT name, parent_id IS NULL FROM chain ORDER BY depth DESC",
        )?;
        let rows = statement
            .query_map(params![account, item_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Outermost ancestor first; it must be the root.
        match rows.split_first() {
            Some(((_, true), [])) => Ok(Some("/".to_owned())),
            Some(((_, true), rest)) => Ok(Some(
                rest.iter().map(|(name, _)| format!("/{name}")).collect(),
            )),
            _ => Ok(None),
        }
    }
}

fn is_folder(tx: &Transaction, account: &str, id: &str) -> Result<Option<bool>> {
    Ok(tx
        .query_row(
            "SELECT is_folder FROM items WHERE account = ?1 AND id = ?2",
            [account, id],
            |row| row.get(0),
        )
        .optional()?)
}

fn has_children(tx: &Transaction, account: &str, id: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM items WHERE account = ?1 AND parent_id = ?2)",
        [account, id],
        |row| row.get(0),
    )?)
}

fn delete(tx: &Transaction, account: &str, id: &str) -> Result<usize> {
    Ok(tx.execute(
        "DELETE FROM items WHERE account = ?1 AND id = ?2",
        [account, id],
    )?)
}

fn upsert(tx: &Transaction, account: &str, item: &RemoteItem) -> Result<()> {
    tx.execute(
        "INSERT INTO items (account, id, parent_id, name, is_folder, size, version, hash, modified)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (account, id) DO UPDATE SET
             parent_id = excluded.parent_id,
             name = excluded.name,
             is_folder = excluded.is_folder,
             size = excluded.size,
             version = excluded.version,
             hash = excluded.hash,
             modified = excluded.modified",
        params![
            account,
            item.id,
            item.parent_id,
            item.name,
            item.is_folder,
            item.size.map(|size| size as i64),
            item.version,
            item.hash,
            item.modified,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRIVE: &str = "onedrive:abcdef0123456789";

    fn item(
        id: &str,
        name: &str,
        parent: Option<&str>,
        is_folder: bool,
        size: Option<u64>,
    ) -> Change {
        Change::Upsert(RemoteItem {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            name: name.to_owned(),
            is_folder,
            size,
            version: None,
            hash: None,
            modified: None,
        })
    }

    fn root() -> Change {
        item("root", "root", None, true, None)
    }

    fn folder(id: &str, name: &str, parent: &str) -> Change {
        item(id, name, Some(parent), true, None)
    }

    fn file(id: &str, name: &str, parent: &str, size: u64) -> Change {
        item(id, name, Some(parent), false, Some(size))
    }

    fn deleted(id: &str) -> Change {
        Change::Delete { id: id.to_owned() }
    }

    fn set(changes: Vec<Change>, cursor: &str, full: bool) -> ChangeSet {
        ChangeSet {
            changes,
            cursor: cursor.to_owned(),
            full,
        }
    }

    fn seeded() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let items = vec![
            root(),
            folder("d1", "Docs", "root"),
            folder("d2", "Work", "d1"),
            file("f1", "a.txt", "d2", 10),
            file("f2", "b.txt", "root", 5),
        ];
        store.apply(DRIVE, set(items, "link-1", true)).unwrap();
        store
    }

    #[test]
    fn full_enumeration_stores_items_and_link() {
        let store = seeded();
        assert_eq!(store.cursor(DRIVE).unwrap().as_deref(), Some("link-1"));
        assert_eq!(
            store.totals(DRIVE).unwrap(),
            Totals {
                folders: 2,
                files: 2,
                bytes: 15
            }
        );
        assert_eq!(store.path(DRIVE, "root").unwrap().as_deref(), Some("/"));
        assert_eq!(
            store.path(DRIVE, "f1").unwrap().as_deref(),
            Some("/Docs/Work/a.txt")
        );
    }

    #[test]
    fn accounts_do_not_share_items_or_cursors() {
        let store = seeded();
        let other = "gdrive:12345";
        assert_eq!(store.cursor(other).unwrap(), None);
        assert_eq!(store.totals(other).unwrap(), Totals::default());
        assert_eq!(store.path(other, "f1").unwrap(), None);
    }

    #[test]
    fn unknown_account_has_no_cursor() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.cursor(DRIVE).unwrap(), None);
    }

    #[test]
    fn renaming_a_folder_moves_descendants_that_delta_did_not_mention() {
        let mut store = seeded();
        let applied = store
            .apply(
                DRIVE,
                set(vec![folder("d1", "Documents", "root")], "link-2", false),
            )
            .unwrap();
        assert_eq!(applied.upserted, 1);
        assert_eq!(
            store.path(DRIVE, "f1").unwrap().as_deref(),
            Some("/Documents/Work/a.txt")
        );
        assert_eq!(store.cursor(DRIVE).unwrap().as_deref(), Some("link-2"));
    }

    #[test]
    fn moving_a_file_changes_its_parent() {
        let mut store = seeded();
        store
            .apply(
                DRIVE,
                set(vec![file("f1", "a.txt", "root", 10)], "link-2", false),
            )
            .unwrap();
        assert_eq!(store.path(DRIVE, "f1").unwrap().as_deref(), Some("/a.txt"));
    }

    #[test]
    fn deleted_folder_tree_is_removed_whatever_the_order() {
        let mut store = seeded();
        // Parent listed before its child and grandchild.
        let applied = store
            .apply(
                DRIVE,
                set(
                    vec![deleted("d1"), deleted("d2"), deleted("f1")],
                    "link-2",
                    false,
                ),
            )
            .unwrap();
        assert_eq!(applied.deleted, 3);
        assert_eq!(applied.folders_kept, 0);
        assert_eq!(
            store.totals(DRIVE).unwrap(),
            Totals {
                folders: 0,
                files: 1,
                bytes: 5
            }
        );
    }

    #[test]
    fn deleted_folder_that_still_has_children_is_kept() {
        let mut store = seeded();
        let applied = store
            .apply(DRIVE, set(vec![deleted("d2")], "link-2", false))
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                folders_kept: 1,
                ..Applied::default()
            }
        );
        assert!(store.path(DRIVE, "f1").unwrap().is_some());
    }

    #[test]
    fn deleting_an_unknown_item_is_a_no_op() {
        let mut store = seeded();
        let applied = store
            .apply(DRIVE, set(vec![deleted("nope")], "link-2", false))
            .unwrap();
        assert_eq!(applied, Applied::default());
    }

    #[test]
    fn full_resync_drops_items_the_service_no_longer_lists() {
        let mut store = seeded();
        let applied = store
            .apply(
                DRIVE,
                set(vec![root(), file("f2", "b.txt", "root", 7)], "link-2", true),
            )
            .unwrap();
        assert_eq!(applied.upserted, 2);
        assert_eq!(applied.deleted, 3);
        assert_eq!(
            store.totals(DRIVE).unwrap(),
            Totals {
                folders: 0,
                files: 1,
                bytes: 7
            }
        );
    }

    #[test]
    fn incremental_set_does_not_drop_unmentioned_items() {
        let mut store = seeded();
        store.apply(DRIVE, set(vec![], "link-2", false)).unwrap();
        assert_eq!(store.totals(DRIVE).unwrap().files, 2);
    }

    #[test]
    fn path_is_none_when_ancestry_does_not_reach_the_root() {
        let mut store = seeded();
        store
            .apply(
                DRIVE,
                set(vec![file("z", "z.txt", "missing", 1)], "link-2", false),
            )
            .unwrap();
        assert_eq!(store.path(DRIVE, "z").unwrap(), None);
        assert_eq!(store.path(DRIVE, "unknown").unwrap(), None);
    }

    #[test]
    fn paths_resolve_to_items_and_folders_list_their_children() {
        let store = seeded();
        let file = store.resolve(DRIVE, "/Docs/Work/a.txt").unwrap().unwrap();
        assert_eq!((file.id.as_str(), file.size), ("f1", Some(10)));
        assert_eq!(store.resolve(DRIVE, "").unwrap().unwrap().id, "root");
        assert_eq!(store.resolve(DRIVE, "/Docs/missing").unwrap(), None);
        assert_eq!(
            store.resolve(DRIVE, "/docs").unwrap(),
            None,
            "names are exact"
        );

        let names: Vec<_> = store
            .children(DRIVE, "root")
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, ["Docs", "b.txt"], "folders come first");
    }

    #[test]
    fn known_account_is_set_replaced_and_cleared() {
        let store = seeded();
        assert_eq!(store.known_account("onedrive").unwrap(), None);
        store.set_known_account("onedrive", Some("a")).unwrap();
        store.set_known_account("onedrive", Some("b")).unwrap();
        assert_eq!(
            store.known_account("onedrive").unwrap().as_deref(),
            Some("b")
        );
        store.set_known_account("onedrive", None).unwrap();
        assert_eq!(store.known_account("onedrive").unwrap(), None);
    }

    #[test]
    fn version_one_database_is_upgraded_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO accounts (key, cursor) VALUES ('k', 'c')", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        let store = Store::init(conn).unwrap();
        assert_eq!(
            store.cursor("k").unwrap().as_deref(),
            Some("c"),
            "data kept"
        );
        store.set_known_account("gdrive", Some("x")).unwrap();
    }

    #[test]
    fn state_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("skydock-state-test-{}", std::process::id()));
        let path = dir.join("nested/state.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .apply(DRIVE, set(vec![root()], "link-1", true))
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.cursor(DRIVE).unwrap().as_deref(), Some("link-1"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
