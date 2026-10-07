//! OneDrive (personal Microsoft accounts) through Microsoft Graph.

mod delta;
pub mod model;
mod upload;

use std::path::Path;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::json;
use skydock_core::http::{ApiClient, read_json};
use skydock_core::oauth::{Authenticator, OAuthConfig};
use skydock_core::{
    Account, Change, ChangeSet, Downloaded, Error, HashKind, PageCallback, Provider, ProviderKind,
    RemoteItem, Result, UploadTarget, UrlCallback,
};

use crate::delta::{DeltaLink, Page};
use crate::model::{Drive, DriveItem, User, normalize_drive_id};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Only what `DriveItem` reads, in large pages: the full enumeration of a
/// big drive is otherwise dominated by payload we throw away.
const ITEM_QUERY: &str = "$top=1000&$select=id,name,size,eTag,cTag,parentReference,\
                          fileSystemInfo,file,folder,root,deleted,remoteItem";

/// One page of a folder's children.
#[derive(serde::Deserialize)]
struct Children {
    #[serde(default)]
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

pub struct OneDrive {
    api: ApiClient,
}

impl OneDrive {
    /// The current version of the file `target` replaces, if its content is
    /// still the one the upload was prepared on.
    async fn same_content_version(&self, target: UploadTarget<'_>) -> Result<Option<String>> {
        let UploadTarget::Replace {
            item_id,
            base_hash: Some(base_hash),
            ..
        } = target
        else {
            return Ok(None);
        };
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}?$select=id,eTag,file");
        let current: DriveItem = self.api.get_json(&url).await?;
        Ok(match current.quick_xor_hash() == Some(base_hash) {
            true => current.e_tag,
            false => None,
        })
    }

    /// `client_id` is the Entra application (client) ID.
    pub fn new(client_id: String) -> Result<Self> {
        let auth = Authenticator::new(OAuthConfig {
            provider: ProviderKind::OneDrive,
            // Personal accounts only, hence the `consumers` tenant.
            authorize_url: "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize",
            token_url: "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
            client_id,
            client_secret: None,
            scopes: "Files.ReadWrite User.Read offline_access",
            redirect_host: "localhost",
            extra_authorize_params: &[("response_mode", "query"), ("prompt", "select_account")],
        })?;
        Ok(Self {
            api: ApiClient::new(auth),
        })
    }
}

#[async_trait]
impl Provider for OneDrive {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OneDrive
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
        let drive: Drive = self.api.get_json(&format!("{GRAPH_BASE}/me/drive")).await?;
        let user: User = self
            .api
            .get_json(&format!(
                "{GRAPH_BASE}/me?$select=displayName,userPrincipalName,mail"
            ))
            .await?;
        let quota = drive.quota.as_ref();
        Ok(Account {
            id: normalize_drive_id(&drive.id),
            display_name: user.display_name,
            email: user.mail.or(user.user_principal_name),
            quota_used: quota.and_then(|q| q.used),
            quota_total: quota.and_then(|q| q.total),
        })
    }

    async fn changes(&self, cursor: Option<&str>, on_page: &PageCallback<'_>) -> Result<ChangeSet> {
        let (mut url, mut full) = match cursor {
            Some(link) => (link.to_owned(), false),
            None => (
                format!("{GRAPH_BASE}/me/drive/root/delta?{ITEM_QUERY}"),
                true,
            ),
        };
        let mut changes = Vec::new();
        loop {
            match delta::fetch_page(&self.api, &url).await? {
                Page::Items { items, link } => {
                    let fetched = changes.len();
                    changes.extend(items.into_iter().filter_map(|item| item.into_change()));
                    on_page(&changes[fetched..]);
                    match link {
                        DeltaLink::Next(next) => url = next,
                        DeltaLink::Delta(cursor) => {
                            return Ok(ChangeSet {
                                changes,
                                cursor,
                                full,
                            });
                        }
                    }
                }
                // Nothing fetched under the dead link can be trusted.
                Page::Resync { location } if !full => {
                    changes.clear();
                    url = location;
                    full = true;
                }
                Page::Resync { .. } => {
                    return Err(Error::Api {
                        status: StatusCode::GONE,
                        body: "the service rejected a fresh delta enumeration".to_owned(),
                    });
                }
            }
        }
    }

    async fn top_level(&self) -> Result<Vec<RemoteItem>> {
        let root: DriveItem = self
            .api
            .get_json(&format!("{GRAPH_BASE}/me/drive/root?{ITEM_QUERY}"))
            .await?;
        let mut items = vec![root.into_remote_item()?];
        let mut url = format!("{GRAPH_BASE}/me/drive/root/children?{ITEM_QUERY}");
        loop {
            let page: Children = self.api.get_json(&url).await?;
            items.extend(
                page.value
                    .into_iter()
                    .filter_map(|item| match item.into_change() {
                        Some(Change::Upsert(item)) => Some(item),
                        _ => None,
                    }),
            );
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(items),
            }
        }
    }

    fn hash_kind(&self) -> HashKind {
        HashKind::QuickXor
    }

    async fn download(&self, item_id: &str, dest: &Path) -> Result<Downloaded> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}/content");
        self.api.download(&url, dest, self.hash_kind()).await
    }

    fn accepts_name(&self, name: &str) -> bool {
        upload::accepts_name(name)
    }

    fn names_are_case_sensitive(&self) -> bool {
        false
    }

    async fn upload(&self, target: UploadTarget<'_>, source: &Path) -> Result<RemoteItem> {
        let uploaded = match upload::upload(&self.api, target, source).await {
            // The version also moves when only metadata changes, sometimes
            // by the service's own doing. Only different content counts.
            Err(Error::Conflict) => match self.same_content_version(target).await? {
                Some(version) => {
                    let UploadTarget::Replace {
                        item_id, base_hash, ..
                    } = target
                    else {
                        return Err(Error::Conflict);
                    };
                    let retry = UploadTarget::Replace {
                        item_id,
                        base_version: Some(&version),
                        base_hash,
                    };
                    upload::upload(&self.api, retry, source).await
                }
                None => Err(Error::Conflict),
            },
            other => other,
        };
        uploaded?.into_remote_item()
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteItem> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{parent_id}/children");
        let body = json!({
            "name": name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail",
        });
        let response = self.api.send(|http| http.post(&url).json(&body)).await?;
        read_json::<DriveItem>(response).await?.into_remote_item()
    }

    async fn move_item(
        &self,
        item_id: &str,
        _from_parent_id: &str,
        to_parent_id: &str,
        name: &str,
    ) -> Result<RemoteItem> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}");
        let body = json!({ "name": name, "parentReference": { "id": to_parent_id } });
        let response = self.api.send(|http| http.patch(&url).json(&body)).await?;
        read_json::<DriveItem>(response).await?.into_remote_item()
    }

    async fn delete(&self, item_id: &str) -> Result<()> {
        let url = format!("{GRAPH_BASE}/me/drive/items/{item_id}");
        let response = self.api.send(|http| http.delete(&url)).await?;
        match response.status() {
            status if status.is_success() || status == StatusCode::NOT_FOUND => Ok(()),
            status => Err(Error::Api {
                status,
                body: response.text().await?,
            }),
        }
    }
}
