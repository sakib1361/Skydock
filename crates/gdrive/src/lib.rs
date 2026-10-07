//! Google Drive ("My Drive" only) through the Drive v3 API.
//!
//! Unlike OneDrive there is no single call that both enumerates and tracks:
//! a full listing comes from `files.list`, later changes from `changes.list`.
//! The start token is taken before listing so nothing that changes during
//! the listing is missed.

mod upload;

use std::path::Path;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use skydock_core::http::{ApiClient, read_json};
use skydock_core::oauth::{Authenticator, OAuthConfig};
use skydock_core::{
    Account, Change, ChangeSet, Downloaded, Error, HashKind, PageCallback, Provider, ProviderKind,
    RemoteItem, Result, UploadTarget, UrlCallback,
};
use url::Url;

const API: &str = "https://www.googleapis.com/drive/v3";
const FILE_FIELDS: &str = "id,name,mimeType,parents,size,md5Checksum,modifiedTime,version,trashed";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Docs, Sheets, Slides, shortcuts and the like: entries that live only
/// inside Google and have no bytes to download.
const NATIVE_MIME_PREFIX: &str = "application/vnd.google-apps.";
const PAGE_SIZE: &str = "1000";

pub struct GoogleDrive {
    api: ApiClient,
}

impl GoogleDrive {
    /// Credentials of a Google Cloud OAuth client of type "Desktop app".
    /// Google issues such clients a secret and requires it when exchanging
    /// tokens, although it is not confidential for installed apps.
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let auth = Authenticator::new(OAuthConfig {
            provider: ProviderKind::GoogleDrive,
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            client_id,
            client_secret: Some(client_secret),
            scopes: "https://www.googleapis.com/auth/drive",
            redirect_host: "127.0.0.1",
            // Without these Google returns no refresh token on repeat sign-ins.
            extra_authorize_params: &[("access_type", "offline"), ("prompt", "consent")],
        })?;
        Ok(Self {
            api: ApiClient::new(auth),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T> {
        let url = Url::parse_with_params(&format!("{API}/{path}"), params)
            .expect("static API URL is valid");
        self.api.get_json(url.as_str()).await
    }

    /// A request that changes one file's metadata and returns the result.
    async fn write(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, &str)],
        body: serde_json::Value,
    ) -> Result<RemoteItem> {
        let mut params = params.to_vec();
        params.push(("fields", FILE_FIELDS));
        let url = Url::parse_with_params(&format!("{API}/{path}"), &params)
            .expect("static API URL is valid");
        let response = self
            .api
            .send(|http| http.request(method.clone(), url.as_str()).json(&body))
            .await?;
        read_json::<File>(response).await?.into_item()
    }

    async fn root_id(&self) -> Result<String> {
        let root: File = self.get("files/root", &[("fields", "id")]).await?;
        Ok(root.id)
    }

    /// Everything not in the bin that matches `query`, as upserts, handed
    /// to `on_page` a page at a time.
    async fn list(
        &self,
        query: &str,
        root_id: &str,
        changes: &mut Vec<Change>,
        on_page: &PageCallback<'_>,
    ) -> Result<()> {
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut page_token: Option<String> = None;
        loop {
            let mut params = vec![
                ("q", query),
                ("spaces", "drive"),
                ("pageSize", PAGE_SIZE),
                ("fields", fields.as_str()),
            ];
            if let Some(token) = &page_token {
                params.push(("pageToken", token));
            }
            let page: FileList = self.get("files", &params).await?;
            // Only items that sit in the tree; a full listing has no deletes.
            let fetched = changes.len();
            changes.extend(
                page.files
                    .into_iter()
                    .map(|file| file.into_change(root_id))
                    .filter(|change| matches!(change, Change::Upsert(_))),
            );
            on_page(&changes[fetched..]);
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(()),
            }
        }
    }

    async fn enumerate(&self, on_page: &PageCallback<'_>) -> Result<ChangeSet> {
        // Token first: changes made while we list are then replayed by the
        // next incremental call instead of being lost.
        let start: StartToken = self.get("changes/startPageToken", &[]).await?;
        let root_id = self.root_id().await?;

        let mut changes = vec![Change::Upsert(root_item(&root_id))];
        on_page(&changes);
        self.list("trashed = false", &root_id, &mut changes, on_page)
            .await?;
        Ok(ChangeSet {
            changes,
            cursor: start.start_page_token,
            full: true,
        })
    }

    async fn changes_since(&self, cursor: &str, on_page: &PageCallback<'_>) -> Result<ChangeSet> {
        let root_id = self.root_id().await?;
        let fields =
            format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))");
        let mut changes = Vec::new();
        let mut page_token = cursor.to_owned();
        loop {
            let page: ChangeList = self
                .get(
                    "changes",
                    &[
                        ("pageToken", page_token.as_str()),
                        ("spaces", "drive"),
                        ("pageSize", PAGE_SIZE),
                        ("fields", fields.as_str()),
                    ],
                )
                .await?;
            let fetched = changes.len();
            changes.extend(
                page.changes
                    .into_iter()
                    .filter_map(|change| change.into_change(&root_id)),
            );
            on_page(&changes[fetched..]);
            match (page.next_page_token, page.new_start_page_token) {
                (Some(next), _) => page_token = next,
                (None, Some(cursor)) => {
                    return Ok(ChangeSet {
                        changes,
                        cursor,
                        full: false,
                    });
                }
                (None, None) => {
                    return Err(Error::Api {
                        status: StatusCode::OK,
                        body: "changes page has neither nextPageToken nor newStartPageToken"
                            .to_owned(),
                    });
                }
            }
        }
    }
}

#[async_trait]
impl Provider for GoogleDrive {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GoogleDrive
    }

    async fn sign_in(&self, on_url: &UrlCallback) -> Result<()> {
        self.api.auth().sign_in(on_url).await
    }

    async fn sign_out(&self) -> Result<()> {
        self.api.auth().sign_out().await
    }

    async fn is_signed_in(&self) -> Result<bool> {
        self.api.auth().is_signed_in().await
    }

    async fn account(&self) -> Result<Account> {
        let about: About = self
            .get(
                "about",
                &[(
                    "fields",
                    "user(displayName,emailAddress,permissionId),storageQuota(limit,usage)",
                )],
            )
            .await?;
        let quota = about.storage_quota.unwrap_or_default();
        Ok(Account {
            id: about.user.permission_id,
            display_name: about.user.display_name,
            email: about.user.email_address,
            quota_used: quota.usage.and_then(|n| n.parse().ok()),
            // Absent for accounts with unlimited storage.
            quota_total: quota.limit.and_then(|n| n.parse().ok()),
        })
    }

    async fn changes(&self, cursor: Option<&str>, on_page: &PageCallback<'_>) -> Result<ChangeSet> {
        let Some(cursor) = cursor else {
            return self.enumerate(on_page).await;
        };
        match self.changes_since(cursor, on_page).await {
            // The token is too old or otherwise unusable: start over.
            Err(Error::Api {
                status: StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::GONE,
                ..
            }) => self.enumerate(on_page).await,
            other => other,
        }
    }

    async fn top_level(&self) -> Result<Vec<RemoteItem>> {
        let root_id = self.root_id().await?;
        let mut changes = vec![Change::Upsert(root_item(&root_id))];
        let query = format!("'{root_id}' in parents and trashed = false");
        self.list(&query, &root_id, &mut changes, &|_| {}).await?;
        Ok(changes
            .into_iter()
            .filter_map(|change| match change {
                Change::Upsert(item) => Some(item),
                Change::Delete { .. } => None,
            })
            .collect())
    }

    fn hash_kind(&self) -> HashKind {
        HashKind::Md5
    }

    /// Fails for Google-native documents, which have no content to fetch.
    async fn download(&self, item_id: &str, dest: &Path) -> Result<Downloaded> {
        let url = format!("{API}/files/{item_id}?alt=media");
        self.api.download(&url, dest, self.hash_kind()).await
    }

    fn accepts_name(&self, name: &str) -> bool {
        !name.is_empty()
    }

    /// Google Drive even allows identical names side by side.
    fn names_are_case_sensitive(&self) -> bool {
        true
    }

    async fn upload(&self, target: UploadTarget<'_>, source: &Path) -> Result<RemoteItem> {
        if let UploadTarget::Replace {
            item_id,
            base_version,
            base_hash,
        } = target
        {
            // The API has no conditional update, so look right before
            // sending. A change in the moment between is not caught.
            let current: File = self
                .get(
                    &format!("files/{item_id}"),
                    &[("fields", "id,version,md5Checksum")],
                )
                .await?;
            let same_version = base_version.is_some() && current.version.as_deref() == base_version;
            let same_content = base_hash.is_some() && current.md5_checksum.as_deref() == base_hash;
            if !same_version && !same_content {
                return Err(Error::Conflict);
            }
        }
        upload::upload(&self.api, target, source).await?.into_item()
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteItem> {
        let body = json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent_id] });
        self.write(reqwest::Method::POST, "files", &[], body).await
    }

    async fn move_item(
        &self,
        item_id: &str,
        from_parent_id: &str,
        to_parent_id: &str,
        name: &str,
    ) -> Result<RemoteItem> {
        let parents = [
            ("addParents", to_parent_id),
            ("removeParents", from_parent_id),
        ];
        let params: &[_] = match from_parent_id == to_parent_id {
            true => &[],
            false => &parents,
        };
        let path = format!("files/{item_id}");
        self.write(
            reqwest::Method::PATCH,
            &path,
            params,
            json!({ "name": name }),
        )
        .await
    }

    /// Moves the item to the bin rather than deleting it for good.
    async fn delete(&self, item_id: &str) -> Result<()> {
        let url = format!("{API}/files/{item_id}?fields=id");
        let body = json!({ "trashed": true });
        let response = self.api.send(|http| http.patch(&url).json(&body)).await?;
        match response.status() {
            status if status.is_success() || status == StatusCode::NOT_FOUND => Ok(()),
            status => Err(Error::Api {
                status,
                body: response.text().await?,
            }),
        }
    }
}

/// "My Drive" itself, which the listing calls do not return.
fn root_item(root_id: &str) -> RemoteItem {
    RemoteItem {
        id: root_id.to_owned(),
        parent_id: None,
        name: "My Drive".to_owned(),
        is_folder: true,
        size: None,
        version: None,
        hash: None,
        modified: None,
    }
}

// Google sends 64-bit numbers as JSON strings, hence `String` for sizes,
// versions and quota figures.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    id: String,
    name: Option<String>,
    mime_type: Option<String>,
    #[serde(default)]
    parents: Vec<String>,
    size: Option<String>,
    md5_checksum: Option<String>,
    modified_time: Option<String>,
    version: Option<String>,
    #[serde(default)]
    trashed: bool,
}

impl File {
    /// The file a write returned, which is never the root and never trashed.
    fn into_item(self) -> Result<RemoteItem> {
        match self.into_change("") {
            Change::Upsert(item) => Ok(item),
            Change::Delete { .. } => Err(Error::Api {
                status: StatusCode::OK,
                body: "the service did not describe the file it stored".to_owned(),
            }),
        }
    }

    fn into_change(self, root_id: &str) -> Change {
        let is_root = self.id == root_id;
        let parent_id = self.parents.into_iter().next();
        // Trashed, or no longer anywhere in My Drive (for example a file
        // that is merely shared with the user): not part of the tree.
        let (false, true, Some(name)) = (self.trashed, is_root || parent_id.is_some(), self.name)
        else {
            return Change::Delete { id: self.id };
        };
        let is_folder = is_root || self.mime_type.as_deref() == Some(FOLDER_MIME);
        let is_native = !is_folder
            && self
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with(NATIVE_MIME_PREFIX));
        Change::Upsert(RemoteItem {
            id: self.id,
            parent_id: if is_root { None } else { parent_id },
            name,
            is_folder,
            // Google reports a storage size for native documents, but there
            // is nothing to download; no size and no hash marks them so.
            size: if is_native {
                None
            } else {
                self.size.and_then(|n| n.parse().ok())
            },
            version: self.version,
            hash: self.md5_checksum,
            modified: self.modified_time,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    next_page_token: Option<String>,
    #[serde(default)]
    files: Vec<File>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChange {
    /// Absent for changes that concern a shared drive rather than a file.
    file_id: Option<String>,
    #[serde(default)]
    removed: bool,
    file: Option<File>,
}

impl RawChange {
    fn into_change(self, root_id: &str) -> Option<Change> {
        let id = self.file_id?;
        match self.file {
            Some(file) if !self.removed => Some(file.into_change(root_id)),
            _ => Some(Change::Delete { id }),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangeList {
    next_page_token: Option<String>,
    new_start_page_token: Option<String>,
    #[serde(default)]
    changes: Vec<RawChange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartToken {
    start_page_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct About {
    user: AboutUser,
    storage_quota: Option<StorageQuota>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AboutUser {
    display_name: Option<String>,
    email_address: Option<String>,
    permission_id: String,
}

#[derive(Deserialize, Default)]
struct StorageQuota {
    limit: Option<String>,
    usage: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn file(value: serde_json::Value) -> Change {
        serde_json::from_value::<File>(value)
            .unwrap()
            .into_change("ROOT")
    }

    #[test]
    fn regular_file_keeps_size_hash_and_first_parent() {
        let change = file(json!({
            "id": "f", "name": "a.txt", "mimeType": "text/plain", "parents": ["p1", "p2"],
            "size": "1234", "md5Checksum": "abc", "version": "7",
            "modifiedTime": "2026-01-01T00:00:00Z"
        }));
        let Change::Upsert(item) = change else {
            panic!("expected upsert")
        };
        assert_eq!(item.parent_id.as_deref(), Some("p1"));
        assert_eq!(item.size, Some(1234));
        assert_eq!(item.hash.as_deref(), Some("abc"));
        assert!(!item.is_folder);
    }

    #[test]
    fn folder_is_recognised_by_mime_type() {
        let change = file(json!({
            "id": "d", "name": "Docs", "mimeType": FOLDER_MIME, "parents": ["ROOT"]
        }));
        assert!(matches!(change, Change::Upsert(item) if item.is_folder));
    }

    #[test]
    fn native_document_has_no_size_or_hash() {
        // Google does report a size for these; it must not be passed on.
        let change = file(json!({
            "id": "g", "name": "Notes", "mimeType": "application/vnd.google-apps.document",
            "parents": ["ROOT"], "size": "2048"
        }));
        assert!(
            matches!(change, Change::Upsert(item) if item.size.is_none() && item.hash.is_none())
        );
    }

    #[test]
    fn root_is_a_parentless_folder() {
        let change = file(json!({ "id": "ROOT", "name": "My Drive", "mimeType": FOLDER_MIME }));
        assert!(
            matches!(change, Change::Upsert(item) if item.is_folder && item.parent_id.is_none())
        );
    }

    #[test]
    fn trashed_or_parentless_files_are_deletes() {
        let trashed = file(json!({ "id": "t", "name": "t", "parents": ["ROOT"], "trashed": true }));
        let shared_with_me = file(json!({ "id": "s", "name": "s" }));
        assert_eq!(trashed, Change::Delete { id: "t".to_owned() });
        assert_eq!(shared_with_me, Change::Delete { id: "s".to_owned() });
    }

    #[test]
    fn change_list_distinguishes_removed_updated_and_non_file_changes() {
        let list: ChangeList = serde_json::from_value(json!({
            "newStartPageToken": "42",
            "changes": [
                { "fileId": "gone", "removed": true },
                { "fileId": "f", "removed": false,
                  "file": { "id": "f", "name": "a", "parents": ["ROOT"] } },
                { "changeType": "drive", "driveId": "x" }
            ]
        }))
        .unwrap();
        assert_eq!(list.new_start_page_token.as_deref(), Some("42"));
        let changes: Vec<_> = list
            .changes
            .into_iter()
            .filter_map(|change| change.into_change("ROOT"))
            .collect();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            changes[0],
            Change::Delete {
                id: "gone".to_owned()
            }
        );
        assert!(matches!(&changes[1], Change::Upsert(item) if item.id == "f"));
    }
}
