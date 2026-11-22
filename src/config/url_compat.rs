//! `[url_compat]`: accept the entry URLs of an nginx-vod-module deployment
//! (`docs/technical-design/0019-url-compatibility.md`).

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawUrlCompat {
    enabled: bool,
    path_prefix: String,
}

/// The settings of an enabled `[url_compat]`.
#[derive(Debug, Clone)]
pub(crate) struct UrlCompatSettings {
    /// Stripped from the front of a path before `/hls` or `/dash`, such as `/vod`.
    pub(crate) path_prefix: String,
}

impl RawUrlCompat {
    /// `None` when the section is absent or `enabled = false`.
    pub(super) fn validate(self) -> Result<Option<UrlCompatSettings>> {
        if !self.enabled {
            return Ok(None);
        }
        let prefix = self.path_prefix.trim_end_matches('/');
        let plain = self.path_prefix.is_empty()
            || (!self.path_prefix.contains("//")
                && (prefix.is_empty()
                    || (prefix.starts_with('/')
                        && !prefix.contains(['?', '#', '%', '\\'])
                        && prefix
                            .split('/')
                            .skip(1)
                            .all(|part| !matches!(part, "." | "..") && !part.is_empty())
                        && prefix.bytes().all(|byte| byte.is_ascii_graphic()))));
        if !plain {
            return Err(Error::Configuration(
                "url_compat.path_prefix must be empty or a plain absolute path such as \"/vod\""
                    .to_owned(),
            ));
        }
        Ok(Some(UrlCompatSettings {
            path_prefix: prefix.to_owned(),
        }))
    }
}
