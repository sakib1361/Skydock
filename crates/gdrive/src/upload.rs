//! Sending content through a resumable upload: one request opens a session,
//! then the bytes follow in chunks.

use std::io::SeekFrom;
use std::path::Path;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE};
use serde_json::json;
use skydock_core::http::{ApiClient, read_chunk, read_json};
use skydock_core::{Error, Result, UploadTarget};
use tokio::io::AsyncSeekExt;
use url::Url;

use crate::{API, FILE_FIELDS, File};

const UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";

/// Chunks must be multiples of 256 KiB; this is 8 MiB.
const CHUNK_BYTES: usize = 32 * 256 * 1024;

/// Give up when this many chunks in a row add nothing on the server.
const MAX_STALLED_CHUNKS: u32 = 3;

/// Google answers a chunk that is not the last with this status and no
/// `Location`, so the HTTP client does not treat it as a redirect.
const RESUME_INCOMPLETE: StatusCode = StatusCode::PERMANENT_REDIRECT;

pub(crate) async fn upload(
    api: &ApiClient,
    target: UploadTarget<'_>,
    source: &Path,
) -> Result<File> {
    let size = tokio::fs::metadata(source).await?.len();
    if size == 0 {
        return upload_empty(api, target).await;
    }
    let session = open_session(api, target, size).await?;
    let mut file = tokio::fs::File::open(source).await?;
    let mut offset = 0;
    let mut stalled = 0;
    loop {
        file.seek(SeekFrom::Start(offset)).await?;
        let chunk = read_chunk(&mut file, CHUNK_BYTES).await?;
        if chunk.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file shrank while it was being uploaded",
            )
            .into());
        }
        let range = format!("bytes {offset}-{}/{size}", offset + chunk.len() as u64 - 1);
        let response = api
            .send(|http| {
                http.put(&session)
                    .header(CONTENT_RANGE, &range)
                    .body(chunk.clone())
            })
            .await?;
        if response.status() != RESUME_INCOMPLETE {
            return read_json(response).await;
        }
        // The server says how much it kept, which may be less than was sent.
        let received = response
            .headers()
            .get(RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(received_bytes)
            .unwrap_or(0);
        stalled = if received > offset { 0 } else { stalled + 1 };
        if stalled >= MAX_STALLED_CHUNKS {
            return Err(Error::Api {
                status: RESUME_INCOMPLETE,
                body: "the service keeps asking for data it was already sent".to_owned(),
            });
        }
        offset = received;
    }
}

/// Start a session and return the URL the content goes to.
async fn open_session(api: &ApiClient, target: UploadTarget<'_>, size: u64) -> Result<String> {
    let (method, url, metadata) = request_for(target, "resumable");
    let response = api
        .send(|http| {
            http.request(method.clone(), url.as_str())
                .header("X-Upload-Content-Length", size)
                .json(&metadata)
        })
        .await?;
    let status = response.status();
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match location {
        Some(location) if status.is_success() => Ok(location),
        _ => Err(Error::Api {
            status,
            body: response.text().await?,
        }),
    }
}

/// A resumable session cannot carry zero bytes.
async fn upload_empty(api: &ApiClient, target: UploadTarget<'_>) -> Result<File> {
    let response = match target {
        // Metadata alone creates a file without content.
        UploadTarget::New { parent_id, name } => {
            let url = format!("{API}/files?fields={FILE_FIELDS}");
            let metadata = json!({ "name": name, "parents": [parent_id] });
            api.send(|http| http.post(&url).json(&metadata)).await?
        }
        UploadTarget::Replace { .. } => {
            let (method, url, _) = request_for(target, "media");
            api.send(|http| {
                http.request(method.clone(), url.as_str())
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(Vec::new())
            })
            .await?
        }
    };
    read_json(response).await
}

/// Method, URL and metadata body of the request that begins an upload.
fn request_for(
    target: UploadTarget<'_>,
    upload_type: &str,
) -> (reqwest::Method, Url, serde_json::Value) {
    let (method, path, metadata) = match target {
        UploadTarget::New { parent_id, name } => (
            reqwest::Method::POST,
            format!("{UPLOAD_API}/files"),
            json!({ "name": name, "parents": [parent_id] }),
        ),
        UploadTarget::Replace { item_id, .. } => (
            reqwest::Method::PATCH,
            format!("{UPLOAD_API}/files/{item_id}"),
            json!({}),
        ),
    };
    let url = Url::parse_with_params(
        &path,
        &[("uploadType", upload_type), ("fields", FILE_FIELDS)],
    )
    .expect("static API URL is valid");
    (method, url, metadata)
}

/// How many bytes the server holds, from a `Range: bytes=0-N` header.
fn received_bytes(range: &str) -> Option<u64> {
    let (_, last) = range.strip_prefix("bytes=")?.split_once('-')?;
    Some(last.trim().parse::<u64>().ok()? + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_range_gives_the_next_offset() {
        assert_eq!(received_bytes("bytes=0-8388607"), Some(8_388_608));
        assert_eq!(received_bytes("bytes=0-0"), Some(1));
        assert_eq!(received_bytes("nonsense"), None);
    }

    #[test]
    fn chunks_are_a_multiple_of_256_kib() {
        assert_eq!(CHUNK_BYTES % (256 * 1024), 0);
    }

    #[test]
    fn new_files_are_posted_and_existing_ones_patched() {
        let (method, url, metadata) = request_for(
            UploadTarget::New {
                parent_id: "P",
                name: "a.txt",
            },
            "resumable",
        );
        assert_eq!(method, reqwest::Method::POST);
        assert!(
            url.as_str()
                .starts_with(&format!("{UPLOAD_API}/files?uploadType=resumable"))
        );
        assert_eq!(metadata, json!({ "name": "a.txt", "parents": ["P"] }));

        let (method, url, metadata) = request_for(
            UploadTarget::Replace {
                item_id: "F",
                base_version: None,
                base_hash: None,
            },
            "media",
        );
        assert_eq!(method, reqwest::Method::PATCH);
        assert!(
            url.as_str()
                .starts_with(&format!("{UPLOAD_API}/files/F?uploadType=media"))
        );
        assert_eq!(metadata, json!({}));
    }
}
