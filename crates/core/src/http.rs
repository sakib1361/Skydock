use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;

use tokio::io::AsyncWriteExt;

use crate::hash::{HashKind, Hasher};
use crate::oauth::Authenticator;
use crate::{Downloaded, Error, Result};

const MAX_THROTTLE_ATTEMPTS: u32 = 5;

/// Authenticated HTTP access to a provider API, shared by all providers.
pub struct ApiClient {
    auth: Authenticator,
    /// While set and in the future, no request may be sent. Shared by all
    /// callers because providers expect every request to stop when one is
    /// throttled, and throttled requests still count against the quota.
    pause_until: Mutex<Option<Instant>>,
}

impl ApiClient {
    pub fn new(auth: Authenticator) -> Self {
        Self {
            auth,
            pause_until: Mutex::new(None),
        }
    }

    pub fn auth(&self) -> &Authenticator {
        &self.auth
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self.get(url).await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(Error::Api { status, body });
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Stream the body of an authenticated GET into `dest`, hashing it on
    /// the way. Redirects to pre-authenticated download hosts are followed
    /// without the `Authorization` header, which the HTTP client drops when
    /// the host changes.
    pub async fn download(&self, url: &str, dest: &Path, kind: HashKind) -> Result<Downloaded> {
        let mut response = self.get(url).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await?;
            return Err(Error::Api { status, body });
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut hasher = Hasher::new(kind);
        let mut bytes = 0;
        while let Some(chunk) = response.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            bytes += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(Downloaded {
            bytes,
            hash: hasher.finish(),
        })
    }

    /// Authenticated GET. Handles throttling and one token refresh on 401;
    /// every other status is returned to the caller.
    pub async fn get(&self, url: &str) -> Result<Response> {
        let mut refreshed = false;
        let mut throttled = 0;
        loop {
            self.wait_for_pause().await;
            let token = self.auth.access_token().await?;
            let response =
                send_with_retry(|| self.auth.http().get(url).bearer_auth(&token)).await?;

            match response.status() {
                StatusCode::UNAUTHORIZED if !refreshed => {
                    refreshed = true;
                    self.auth.invalidate().await;
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    throttled += 1;
                    if throttled >= MAX_THROTTLE_ATTEMPTS {
                        return Err(Error::Throttled(throttled));
                    }
                    self.pause_for(
                        retry_after(&response)
                            .unwrap_or(Duration::from_secs(5 * u64::from(throttled))),
                    );
                }
                _ => return Ok(response),
            }
        }
    }

    fn pause_for(&self, duration: Duration) {
        let until = Instant::now() + duration;
        let mut pause = self.pause_until.lock().unwrap();
        if pause.is_none_or(|current| current < until) {
            *pause = Some(until);
        }
    }

    async fn wait_for_pause(&self) {
        // Loop because another caller may extend the pause while we sleep.
        loop {
            let until = *self.pause_until.lock().unwrap();
            match until {
                Some(until) if until > Instant::now() => {
                    tokio::time::sleep_until(until.into()).await
                }
                _ => return,
            }
        }
    }
}

const NETWORK_ATTEMPTS: u32 = 3;

/// Send a request, trying again when the connection itself fails (reset,
/// refused, timed out). These are common on long sessions and say nothing
/// about the request; HTTP error statuses are returned as responses.
pub(crate) async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
) -> std::result::Result<Response, reqwest::Error> {
    let mut attempt = 1;
    loop {
        match build().send().await {
            Err(error)
                if attempt < NETWORK_ATTEMPTS
                    && (error.is_connect() || error.is_timeout() || error.is_request()) =>
            {
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn retry_after(response: &Response) -> Option<Duration> {
    let seconds = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    Some(Duration::from_secs(seconds.trim().parse().ok()?))
}
