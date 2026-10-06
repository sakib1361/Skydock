//! Change tracking through `driveItem: delta`.
//!
//! Follow `Next` links until a page carries a `Delta` link. The caller must
//! persist that link only after applying every page of the set.

use std::collections::HashMap;

use reqwest::StatusCode;
use reqwest::header::LOCATION;
use serde::Deserialize;

use crate::client::GRAPH_BASE;
use crate::error::ResyncKind;
use crate::model::DriveItem;
use crate::{Error, GraphClient, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaLink {
    /// More pages remain in the current set.
    Next(String),
    /// The set is complete; use this link to ask for later changes.
    Delta(String),
}

#[derive(Debug)]
pub struct DeltaPage {
    pub items: Vec<DriveItem>,
    pub link: DeltaLink,
}

#[derive(Deserialize)]
struct RawPage {
    #[serde(default)]
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

impl GraphClient {
    /// URL that starts a full enumeration of the signed-in user's drive.
    pub fn delta_start_url() -> String {
        format!("{GRAPH_BASE}/me/drive/root/delta")
    }

    /// Fetch one page. `url` is [`Self::delta_start_url`] or a link from a
    /// previous page or run.
    pub async fn delta_page(&self, url: &str) -> Result<DeltaPage> {
        let response = self.get(url).await?;
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;

        if status == StatusCode::GONE
            && let Some(location) = location
        {
            return Err(Error::DeltaResync {
                location,
                kind: resync_kind(&body),
            });
        }
        if !status.is_success() {
            return Err(Error::Api { status, body });
        }
        parse_page(&body)
    }
}

fn parse_page(body: &str) -> Result<DeltaPage> {
    let raw: RawPage = serde_json::from_str(body)?;
    let link = match (raw.next_link, raw.delta_link) {
        (Some(next), _) => DeltaLink::Next(next),
        (None, Some(delta)) => DeltaLink::Delta(delta),
        (None, None) => {
            return Err(Error::Api {
                status: StatusCode::OK,
                body: "delta page has neither nextLink nor deltaLink".to_owned(),
            });
        }
    };
    Ok(DeltaPage {
        items: raw.value,
        link,
    })
}

fn resync_kind(body: &str) -> ResyncKind {
    // When the code is missing or unrecognised, pick the reconciliation that
    // cannot discard local data.
    if body.contains("resyncChangesApplyDifferences") {
        ResyncKind::ApplyDifferences
    } else {
        ResyncKind::UploadDifferences
    }
}

/// Collapse a delta set to one entry per item. The same item can appear more
/// than once and only its last occurrence is authoritative.
pub fn latest_per_item(items: Vec<DriveItem>) -> Vec<DriveItem> {
    let mut slot_by_id: HashMap<String, usize> = HashMap::new();
    let mut latest: Vec<Option<DriveItem>> = Vec::with_capacity(items.len());
    for item in items {
        if let Some(previous) = slot_by_id.insert(item.id.clone(), latest.len()) {
            latest[previous] = None;
        }
        latest.push(Some(item));
    }
    latest.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str) -> DriveItem {
        serde_json::from_value(serde_json::json!({ "id": id, "name": name })).unwrap()
    }

    #[test]
    fn page_with_next_link() {
        let page = parse_page(
            r#"{"value":[{"id":"1","name":"a","folder":{}}],"@odata.nextLink":"https://n"}"#,
        )
        .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.link, DeltaLink::Next("https://n".to_owned()));
    }

    #[test]
    fn final_page_may_be_empty() {
        let page = parse_page(r#"{"value":[],"@odata.deltaLink":"https://d"}"#).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.link, DeltaLink::Delta("https://d".to_owned()));
    }

    #[test]
    fn page_without_any_link_is_an_error() {
        assert!(parse_page(r#"{"value":[]}"#).is_err());
    }

    #[test]
    fn resync_kind_defaults_to_the_non_destructive_choice() {
        assert_eq!(
            resync_kind(r#"{"error":{"code":"resyncChangesApplyDifferences"}}"#),
            ResyncKind::ApplyDifferences
        );
        assert_eq!(
            resync_kind(r#"{"error":{"code":"resyncChangesUploadDifferences"}}"#),
            ResyncKind::UploadDifferences
        );
        assert_eq!(resync_kind("{}"), ResyncKind::UploadDifferences);
    }

    #[test]
    fn last_occurrence_of_an_item_wins() {
        let latest = latest_per_item(vec![item("1", "old"), item("2", "b"), item("1", "new")]);
        let names: Vec<_> = latest.iter().map(|i| i.name.as_deref().unwrap()).collect();
        assert_eq!(names, ["b", "new"]);
    }
}
