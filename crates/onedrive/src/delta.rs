//! Change tracking through `driveItem: delta`.
//!
//! Follow `Next` links until a page carries a `Delta` link, which is the
//! cursor for the next run.

use reqwest::StatusCode;
use reqwest::header::LOCATION;
use serde::Deserialize;
use skydock_core::http::ApiClient;
use skydock_core::{Error, Result};

use crate::model::DriveItem;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeltaLink {
    /// More pages remain in the current set.
    Next(String),
    /// The set is complete; use this link to ask for later changes.
    Delta(String),
}

pub(crate) enum Page {
    Items {
        items: Vec<DriveItem>,
        link: DeltaLink,
    },
    /// HTTP 410: the link we used is dead. Everything must be enumerated
    /// again starting from `location`.
    Resync { location: String },
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

pub(crate) async fn fetch_page(api: &ApiClient, url: &str) -> Result<Page> {
    let response = api.get(url).await?;
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
        return Ok(Page::Resync { location });
    }
    if !status.is_success() {
        return Err(Error::Api { status, body });
    }
    parse_page(&body)
}

fn parse_page(body: &str) -> Result<Page> {
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
    Ok(Page::Items {
        items: raw.value,
        link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(body: &str) -> (usize, DeltaLink) {
        match parse_page(body).unwrap() {
            Page::Items { items, link } => (items.len(), link),
            Page::Resync { .. } => panic!("unexpected resync"),
        }
    }

    #[test]
    fn page_with_next_link() {
        let page = parsed(
            r#"{"value":[{"id":"1","name":"a","folder":{}}],"@odata.nextLink":"https://n"}"#,
        );
        assert_eq!(page, (1, DeltaLink::Next("https://n".to_owned())));
    }

    #[test]
    fn final_page_may_be_empty() {
        let page = parsed(r#"{"value":[],"@odata.deltaLink":"https://d"}"#);
        assert_eq!(page, (0, DeltaLink::Delta("https://d".to_owned())));
    }

    #[test]
    fn page_without_any_link_is_an_error() {
        assert!(parse_page(r#"{"value":[]}"#).is_err());
    }
}
