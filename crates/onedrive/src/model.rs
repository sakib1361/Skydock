//! The subset of Graph resource shapes this client reads. Unknown fields are
//! ignored, and almost everything is optional because delta omits properties
//! depending on the operation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use skydock_core::{Change, RemoteItem};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    pub drive_type: Option<String>,
    pub quota: Option<Quota>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub display_name: Option<String>,
    pub user_principal_name: Option<String>,
    pub mail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Quota {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub remaining: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    pub parent_reference: Option<ParentReference>,
    pub file_system_info: Option<FileSystemInfo>,
    pub file: Option<FileFacet>,
    pub folder: Option<Value>,
    pub root: Option<Value>,
    pub deleted: Option<Value>,
    /// Present on shortcuts to folders that live in another drive.
    pub remote_item: Option<Value>,
}

impl DriveItem {
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }

    pub fn is_file(&self) -> bool {
        self.file.is_some()
    }

    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file
            .as_ref()?
            .hashes
            .as_ref()?
            .quick_xor_hash
            .as_deref()
    }

    /// Provider-neutral form. `None` for entries that cannot be placed in
    /// the tree (no name, or no parent and not the root).
    pub fn into_change(self) -> Option<Change> {
        if self.is_deleted() {
            return Some(Change::Delete { id: self.id });
        }
        let is_root = self.root.is_some();
        let parent_id = self.parent_reference.as_ref().and_then(|p| p.id.clone());
        if !is_root && parent_id.is_none() {
            return None;
        }
        // Shortcuts to folders in other drives carry no `folder` facet of
        // their own; the facet sits inside `remoteItem`.
        let is_folder = is_root
            || self.is_folder()
            || self
                .remote_item
                .as_ref()
                .is_some_and(|remote| remote.get("folder").is_some());
        let hash = self.quick_xor_hash().map(str::to_owned);
        Some(Change::Upsert(RemoteItem {
            id: self.id,
            parent_id: if is_root { None } else { parent_id },
            name: self.name?,
            is_folder,
            size: self.size,
            version: self.e_tag,
            hash,
            modified: self
                .file_system_info
                .and_then(|info| info.last_modified_date_time),
        }))
    }
}

/// Delta never includes `path` here; items are tracked by ID.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    pub drive_id: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    pub created_date_time: Option<String>,
    pub last_modified_date_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    pub mime_type: Option<String>,
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub quick_xor_hash: Option<String>,
}

/// Canonical form of a drive ID for use as a key. Personal accounts return
/// the same drive with differing case and, for IDs with leading zeros, with
/// 15 instead of 16 hex characters depending on the endpoint.
pub fn normalize_drive_id(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    if id.len() < 16 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("{id:0>16}")
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_ids_are_lowercased_and_padded() {
        assert_eq!(normalize_drive_id("24470056F5C3E43"), "024470056f5c3e43");
        assert_eq!(normalize_drive_id("B4470056F5C3E431"), "b4470056f5c3e431");
    }

    #[test]
    fn non_hex_drive_ids_are_only_lowercased() {
        assert_eq!(normalize_drive_id("b!AbC-123"), "b!abc-123");
    }

    #[test]
    fn deleted_item_without_name_or_size_parses() {
        let item: DriveItem =
            serde_json::from_str(r#"{"id":"A!1","deleted":{"state":"deleted"}}"#).unwrap();
        assert!(item.is_deleted());
        assert!(item.name.is_none() && item.size.is_none());
    }

    #[test]
    fn file_item_exposes_tags_and_hash() {
        let item: DriveItem = serde_json::from_str(
            r#"{"id":"A!2","name":"a.txt","size":5,"eTag":"e","cTag":"c",
                "parentReference":{"driveId":"ABC","id":"A!1"},
                "file":{"hashes":{"quickXorHash":"aGVsbG8="}},
                "@microsoft.graph.downloadUrl":"https://example.invalid/x"}"#,
        )
        .unwrap();
        assert!(item.is_file() && !item.is_folder());
        assert_eq!(item.c_tag.as_deref(), Some("c"));
        assert_eq!(item.quick_xor_hash(), Some("aGVsbG8="));
    }

    fn item(json: serde_json::Value) -> DriveItem {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn deleted_entry_becomes_a_delete() {
        let change = item(serde_json::json!({ "id": "x", "deleted": {} })).into_change();
        assert_eq!(change, Some(Change::Delete { id: "x".to_owned() }));
    }

    #[test]
    fn root_has_no_parent_and_is_a_folder() {
        let change = item(serde_json::json!({
            "id": "r", "name": "root", "root": {}, "parentReference": { "id": "ignored" }
        }))
        .into_change();
        let Some(Change::Upsert(root)) = change else {
            panic!("expected upsert")
        };
        assert!(root.is_folder && root.parent_id.is_none());
    }

    #[test]
    fn shared_folder_shortcut_counts_as_a_folder() {
        let change = item(serde_json::json!({
            "id": "s", "name": "Shared", "parentReference": { "id": "r" },
            "remoteItem": { "id": "other", "folder": {} }
        }))
        .into_change();
        assert!(matches!(change, Some(Change::Upsert(item)) if item.is_folder));
    }

    #[test]
    fn entries_without_name_or_parent_are_dropped() {
        let nameless = item(serde_json::json!({ "id": "a", "parentReference": { "id": "r" } }));
        let orphan = item(serde_json::json!({ "id": "b", "name": "b" }));
        assert_eq!(nameless.into_change(), None);
        assert_eq!(orphan.into_change(), None);
    }
}
