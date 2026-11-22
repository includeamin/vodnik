//! `[fallback]`: pass a miss on to another server
//! (`docs/technical-design/0020-fallback-on-miss.md`).

use reqwest::Url;
use serde::Deserialize;

use crate::error::{Error, Result};

const fn default_true() -> bool {
    true
}

const fn default_timeout_ms() -> u64 {
    5_000
}

fn default_loop_header() -> String {
    "x-vod-fallback".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawFallback {
    enabled: bool,
    base_url: String,
    #[serde(default = "default_true")]
    on_missing_file: bool,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_loop_header")]
    loop_header: String,
}

impl Default for RawFallback {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            on_missing_file: true,
            timeout_ms: default_timeout_ms(),
            loop_header: default_loop_header(),
        }
    }
}

/// The settings of an enabled `[fallback]`.
#[derive(Debug, Clone)]
pub(crate) struct FallbackSettings {
    /// Scheme and authority, plus an optional path prefix, without a trailing slash.
    pub(crate) base_url: Url,
    /// Also forward a request whose asset resolved but whose media file is missing.
    pub(crate) on_missing_file: bool,
    /// Connect timeout, and the longest the fallback may stay silent while answering.
    pub(crate) timeout_ms: u64,
    /// Sent on every forwarded request. A request that already carries it is never forwarded.
    pub(crate) loop_header: String,
}

impl RawFallback {
    /// `None` when the section is absent or `enabled = false`.
    pub(super) fn validate(self) -> Result<Option<FallbackSettings>> {
        if !self.enabled {
            return Ok(None);
        }
        let invalid = |what: &str| Error::Configuration(format!("fallback.{what}"));
        let base_url = Url::parse(self.base_url.trim_end_matches('/'))
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
            .ok_or_else(|| {
                invalid("base_url must be an http(s) URL without a query or credentials")
            })?;
        if self.timeout_ms == 0 {
            return Err(invalid("timeout_ms must be greater than zero"));
        }
        let loop_header = self.loop_header.to_ascii_lowercase();
        if axum::http::HeaderName::from_bytes(loop_header.as_bytes()).is_err() {
            return Err(invalid("loop_header must be a valid HTTP header name"));
        }
        Ok(Some(FallbackSettings {
            base_url,
            on_missing_file: self.on_missing_file,
            timeout_ms: self.timeout_ms,
            loop_header,
        }))
    }
}
