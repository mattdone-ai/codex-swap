use crate::auth;
use anyhow::{Context, Result, bail};
use reqwest::{blocking::Client, header::HeaderMap, redirect::Policy};
use serde_json::Value;
use std::{io::Read, path::Path, time::Duration};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_RETRY_AFTER_SECONDS: u64 = 3600;

#[derive(Debug)]
pub struct RateLimited {
    pub retry_after_seconds: u64,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Codex usage API is rate limited (HTTP 429); retry after {} seconds",
            self.retry_after_seconds
        )
    }
}

impl std::error::Error for RateLimited {}

fn retry_after_seconds_at(headers: &HeaderMap, now: chrono::DateTime<chrono::Utc>) -> u64 {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    value
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            value
                .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
                .map(|retry| retry.timestamp().saturating_sub(now.timestamp()).max(1) as u64)
        })
        .unwrap_or(60)
        .clamp(1, MAX_RETRY_AFTER_SECONDS)
}

fn retry_after_seconds(headers: &HeaderMap) -> u64 {
    retry_after_seconds_at(headers, chrono::Utc::now())
}

pub struct Response {
    pub body: Value,
    pub headers: HeaderMap,
}

pub fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(10))
        .redirect(Policy::none())
        .user_agent(concat!("codex-swap/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("create usage HTTP client")
}

/// CDXC:AgentProviders 2026-09-06 WHY:
/// Codex owns refresh-token rotation in each account home; usage reads its current access token because competing refreshes can invalidate a running Codex login.
/// The endpoint and account header follow OpenUsage's CodexUsageClient; see THIRD_PARTY_NOTICES.md.
pub fn fetch(
    client: &Client,
    home: &Path,
    expected: &Option<auth::Identity>,
) -> Result<(auth::Identity, Response)> {
    let (auth, identity) = auth::verified_credentials(home, expected)?;
    let token = auth["tokens"]["access_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("Codex login has no access token; sign in with xswap login")?;
    let response = client
        .get(USAGE_URL)
        .bearer_auth(token)
        .header("ChatGPT-Account-Id", &identity.account_id)
        .header("Accept", "application/json")
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Codex usage request timed out after 10 seconds")
            } else {
                anyhow::anyhow!("could not connect to Codex usage API")
            }
        })?;
    let status = response.status();
    if status.as_u16() == 401 {
        bail!(
            "Codex access token expired or was rejected; run Codex for this account to refresh its login, or use xswap login <account>"
        );
    }
    if status.as_u16() == 429 {
        return Err(RateLimited {
            retry_after_seconds: retry_after_seconds(response.headers()),
        }
        .into());
    }
    if !status.is_success() {
        bail!("Codex usage request failed (HTTP {})", status.as_u16());
    }
    let headers = response.headers().clone();
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read Codex usage response")?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        bail!("Codex usage response is too large");
    }
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Codex usage API returned invalid JSON (contents omitted)"))?;
    if !body.is_object() {
        bail!("Codex usage API returned an invalid response");
    }
    Ok((identity, Response { body, headers }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded_and_defaults() {
        let mut headers = HeaderMap::new();
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        assert_eq!(retry_after_seconds_at(&headers, now), 60);
        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(retry_after_seconds_at(&headers, now), 1);
        headers.insert(reqwest::header::RETRY_AFTER, "99999".parse().unwrap());
        assert_eq!(retry_after_seconds_at(&headers, now), 3600);
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        assert_eq!(retry_after_seconds_at(&headers, now), 120);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            chrono::DateTime::from_timestamp(1_800_000_120, 0)
                .unwrap()
                .to_rfc2822()
                .parse()
                .unwrap(),
        );
        assert_eq!(retry_after_seconds_at(&headers, now), 120);
    }
}
