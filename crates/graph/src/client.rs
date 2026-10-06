use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;

use crate::model::Drive;
use crate::{Authenticator, Error, Result};

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

const MAX_THROTTLE_ATTEMPTS: u32 = 5;

pub struct GraphClient {
    auth: Authenticator,
    /// While set and in the future, no request may be sent. Shared by all
    /// callers because Graph expects every request to stop when one is
    /// throttled, and throttled requests still count against the quota.
    pause_until: Mutex<Option<Instant>>,
}

impl GraphClient {
    pub fn new(auth: Authenticator) -> Self {
        Self {
            auth,
            pause_until: Mutex::new(None),
        }
    }

    pub fn auth(&self) -> &Authenticator {
        &self.auth
    }

    pub async fn my_drive(&self) -> Result<Drive> {
        self.get_json(&format!("{GRAPH_BASE}/me/drive")).await
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self.get(url).await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(Error::Api { status, body });
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Authenticated GET. Handles throttling and one token refresh on 401;
    /// every other status is returned to the caller.
    pub(crate) async fn get(&self, url: &str) -> Result<Response> {
        let mut refreshed = false;
        let mut throttled = 0;
        loop {
            self.wait_for_pause().await;
            let token = self.auth.access_token().await?;
            let response = self.auth.http().get(url).bearer_auth(token).send().await?;

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

fn retry_after(response: &Response) -> Option<Duration> {
    let seconds = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    Some(Duration::from_secs(seconds.trim().parse().ok()?))
}
