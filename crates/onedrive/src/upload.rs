//! Sending content: one request for small files, an upload session of
//! sequential fragments for the rest.

use std::path::Path;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, IF_MATCH};
use serde::Deserialize;
use serde_json::json;
use skydock_core::http::{ApiClient, read_chunk, read_json};
use skydock_core::{Error, Result, UploadTarget};

use crate::GRAPH_BASE;
use crate::model::DriveItem;

/// Above this a session is used. A zero-byte file cannot go through a
/// session at all.
const SIMPLE_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024;

/// Fragments must be multiples of 320 KiB; this is 10 MiB.
const FRAGMENT_BYTES: usize = 32 * 320 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    upload_url: String,
}

pub(crate) async fn upload(
    api: &ApiClient,
    target: UploadTarget<'_>,
    source: &Path,
) -> Result<DriveItem> {
    let size = tokio::fs::metadata(source).await?.len();
    let response = if size <= SIMPLE_UPLOAD_LIMIT {
        let content = tokio::fs::read(source).await?;
        let url = match target {
            UploadTarget::New { .. } => format!(
                "{}/content?@microsoft.graph.conflictBehavior=rename",
                address(target)
            ),
            UploadTarget::Replace { .. } => format!("{}/content", address(target)),
        };
        api.send(|http| {
            let request = http
                .put(&url)
                .header("Content-Type", "application/octet-stream")
                .body(content.clone());
            with_expected_version(request, target)
        })
        .await?
    } else {
        let session = create_session(api, target).await?;
        let sent = send_fragments(api, &session.upload_url, source, size).await;
        if sent.is_err() {
            // Best effort: an abandoned session otherwise lingers for days.
            let _ = api
                .send_preauthenticated(|http| http.delete(&session.upload_url))
                .await;
        }
        sent?
    };
    match response.status() {
        StatusCode::PRECONDITION_FAILED => Err(Error::Conflict),
        _ => read_json(response).await,
    }
}

async fn create_session(api: &ApiClient, target: UploadTarget<'_>) -> Result<Session> {
    let url = format!("{}/createUploadSession", address(target));
    let behavior = match target {
        UploadTarget::New { .. } => "rename",
        UploadTarget::Replace { .. } => "replace",
    };
    let body = json!({ "item": { "@microsoft.graph.conflictBehavior": behavior } });
    let response = api
        .send(|http| with_expected_version(http.post(&url).json(&body), target))
        .await?;
    match response.status() {
        StatusCode::PRECONDITION_FAILED => Err(Error::Conflict),
        _ => read_json(response).await,
    }
}

/// Returns the response to the last fragment, which describes the item.
async fn send_fragments(
    api: &ApiClient,
    upload_url: &str,
    source: &Path,
    size: u64,
) -> Result<reqwest::Response> {
    let mut file = tokio::fs::File::open(source).await?;
    let mut offset = 0;
    loop {
        let fragment = read_chunk(&mut file, FRAGMENT_BYTES).await?;
        if fragment.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file shrank while it was being uploaded",
            )
            .into());
        }
        let end = offset + fragment.len() as u64 - 1;
        let range = format!("bytes {offset}-{end}/{size}");
        // The session URL carries its own authorisation.
        let response = api
            .send_preauthenticated(|http| {
                http.put(upload_url)
                    .header(CONTENT_RANGE, &range)
                    .body(fragment.clone())
            })
            .await?;
        match response.status() {
            StatusCode::ACCEPTED if end + 1 < size => offset = end + 1,
            StatusCode::ACCEPTED => {
                return Err(Error::Api {
                    status: StatusCode::ACCEPTED,
                    body: "every fragment was sent but the upload did not complete".to_owned(),
                });
            }
            _ => return Ok(response),
        }
    }
}

/// The item an upload is addressed to, without the final path segment.
fn address(target: UploadTarget<'_>) -> String {
    match target {
        UploadTarget::New { parent_id, name } => format!(
            "{GRAPH_BASE}/me/drive/items/{parent_id}:/{}:",
            encode_segment(name)
        ),
        UploadTarget::Replace { item_id, .. } => format!("{GRAPH_BASE}/me/drive/items/{item_id}"),
    }
}

fn with_expected_version(
    request: reqwest::RequestBuilder,
    target: UploadTarget<'_>,
) -> reqwest::RequestBuilder {
    match target {
        UploadTarget::Replace {
            base_version: Some(version),
            ..
        } => request.header(IF_MATCH, version),
        _ => request,
    }
}

/// Percent-encode a name for use as one URL path segment.
fn encode_segment(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Whether OneDrive can store an item under this name. See "Restrictions
/// and limitations in OneDrive and SharePoint".
pub(crate) fn accepts_name(name: &str) -> bool {
    const FORBIDDEN: [char; 9] = ['"', '*', ':', '<', '>', '?', '/', '\\', '|'];
    const RESERVED: [&str; 6] = [".lock", "con", "prn", "aux", "nul", "desktop.ini"];

    let lower = name.to_lowercase();
    let numbered_device = ["com", "lpt"].iter().any(|device| {
        lower
            .strip_prefix(device)
            .is_some_and(|rest| rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit())
    });
    !(name.is_empty()
        || name.chars().count() > 255
        || name.contains(FORBIDDEN)
        || name.chars().any(char::is_control)
        || name.starts_with(' ')
        || name.ends_with([' ', '.'])
        || name.starts_with("~$")
        || lower.contains("_vti_")
        || RESERVED.contains(&lower.as_str())
        || numbered_device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_encoded_for_a_path_segment() {
        assert_eq!(encode_segment("report-1_v2.txt"), "report-1_v2.txt");
        assert_eq!(encode_segment("a b#c%.txt"), "a%20b%23c%25.txt");
        assert_eq!(encode_segment("naïve"), "na%C3%AFve");
    }

    #[test]
    fn new_and_existing_files_are_addressed_differently() {
        let new = UploadTarget::New {
            parent_id: "P!1",
            name: "a b.txt",
        };
        let replace = UploadTarget::Replace {
            item_id: "F!2",
            base_version: None,
            base_hash: None,
        };
        assert_eq!(
            address(new),
            format!("{GRAPH_BASE}/me/drive/items/P!1:/a%20b.txt:")
        );
        assert_eq!(address(replace), format!("{GRAPH_BASE}/me/drive/items/F!2"));
    }

    #[test]
    fn fragments_are_a_multiple_of_320_kib() {
        assert_eq!(FRAGMENT_BYTES % (320 * 1024), 0);
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for name in [
            "notes.txt",
            ".hidden",
            "résumé (final).docx",
            "a#b%c",
            "com",
            "com10",
        ] {
            assert!(accepts_name(name), "{name}");
        }
    }

    #[test]
    fn names_onedrive_cannot_store_are_refused() {
        for name in [
            "",
            "a:b",
            "what?.txt",
            "back\\slash",
            "trailing.",
            "trailing ",
            " leading",
            "~$draft.docx",
            "CON",
            "lpt1",
            "Desktop.ini",
            ".lock",
            "my_vti_file",
            "tab\there",
        ] {
            assert!(!accepts_name(name), "{name:?}");
        }
    }
}
