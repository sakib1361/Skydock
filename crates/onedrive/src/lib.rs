//! OneDrive (personal Microsoft accounts) through Microsoft Graph.

mod delta;
pub mod model;

use std::path::Path;

use async_trait::async_trait;
use skydock_core::http::ApiClient;
use skydock_core::oauth::{Authenticator, OAuthConfig};
use skydock_core::{
    Account, ChangeSet, Downloaded, Error, HashKind, ProgressCallback, Provider, ProviderKind,
    Result, UrlCallback,
};

use crate::delta::{DeltaLink, Page};
use crate::model::{Drive, User, normalize_drive_id};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Only what `DriveItem` reads, in large pages: the full enumeration of a
/// big drive is otherwise dominated by payload we throw away.
const DELTA_QUERY: &str = "$top=1000&$select=id,name,size,eTag,cTag,parentReference,\
                           fileSystemInfo,file,folder,root,deleted,remoteItem";

pub struct OneDrive {
    api: ApiClient,
}

impl OneDrive {
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

    async fn changes(
        &self,
        cursor: Option<&str>,
        progress: &ProgressCallback,
    ) -> Result<ChangeSet> {
        let (mut url, mut full) = match cursor {
            Some(link) => (link.to_owned(), false),
            None => (
                format!("{GRAPH_BASE}/me/drive/root/delta?{DELTA_QUERY}"),
                true,
            ),
        };
        let mut changes = Vec::new();
        loop {
            match delta::fetch_page(&self.api, &url).await? {
                Page::Items { items, link } => {
                    changes.extend(items.into_iter().filter_map(|item| item.into_change()));
                    progress(changes.len());
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
                        status: reqwest::StatusCode::GONE,
                        body: "the service rejected a fresh delta enumeration".to_owned(),
                    });
                }
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
}
