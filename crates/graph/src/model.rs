//! The subset of Graph resource shapes this client reads. Unknown fields are
//! ignored, and almost everything is optional because delta omits properties
//! depending on the operation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    pub drive_type: Option<String>,
    pub quota: Option<Quota>,
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
}
