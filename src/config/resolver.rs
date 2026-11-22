//! Configuration for asset resolution: the resolver choice, the mapper client, the registry
//! caches, and the policy for remote media.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// A credential that never appears in `Debug` output or logs.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// Reads a bearer token from a file: one line of visible ASCII, a trailing newline allowed.
/// The reason never includes the contents.
pub(crate) fn read_token_file(path: &Path) -> std::result::Result<Secret, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("cannot be read: {error}"))?;
    let token = contents.trim_end_matches(['\n', '\r']);
    if token.is_empty() {
        return Err("is empty".to_owned());
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("must be a single line of visible ASCII characters".to_owned());
    }
    Ok(Secret(token.to_owned()))
}

/// Where asset locations come from.
#[derive(Debug, Clone)]
pub(crate) enum ResolverSettings {
    /// The `[assets.*]` catalog in the configuration file.
    Static,
    /// An external mapper service.
    Http(MapperConfig),
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResolverKind {
    #[default]
    Static,
    Http,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct RawResolver {
    #[serde(rename = "type", default)]
    pub(super) kind: ResolverKind,
    pub(super) http: Option<RawMapper>,
}

/// What the mapper service speaks.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// vodnik's own mapper API (`docs/mapper-api.md`).
    #[default]
    #[serde(rename = "native")]
    Native,
    /// An nginx-vod-module mapping service (TDD 0015).
    #[serde(rename = "vod-module")]
    VodModule,
}

impl Dialect {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::VodModule => "vod-module",
        }
    }
}

/// `[resolver.http.vod_module.drm]`, as written (TDD 0018).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawVodDrm {
    path_template: Option<String>,
    extra_args: Option<String>,
    host_header: Option<String>,
    clear_lead_segments: u32,
    max_response_bytes: usize,
}

impl Default for RawVodDrm {
    fn default() -> Self {
        Self {
            path_template: None,
            extra_args: None,
            host_header: None,
            clear_lead_segments: 0,
            max_response_bytes: 16 * 1024,
        }
    }
}

/// Where an nginx-vod-module deployment's key service is, and how its DRM info is used.
#[derive(Debug, Clone)]
pub(crate) struct VodDrmConfig {
    /// After `base_url`; `{id}` is the asset ID, percent-encoded.
    pub(crate) path_template: String,
    pub(crate) extra_args: Option<String>,
    pub(crate) host_header: Option<String>,
    /// How many of an asset's first segments are left clear
    /// (nginx-vod-module's `vod_drm_clear_lead_segment_count`).
    pub(crate) clear_lead_segments: u32,
    pub(crate) max_response_bytes: usize,
}

impl Default for VodDrmConfig {
    fn default() -> Self {
        Self {
            path_template: "{id}".to_owned(),
            extra_args: None,
            host_header: None,
            clear_lead_segments: 0,
            max_response_bytes: 16 * 1024,
        }
    }
}

impl RawVodDrm {
    fn validate(self) -> Result<VodDrmConfig> {
        let fail = |message: &str| Err(Error::Configuration(message.to_owned()));
        let template = self.path_template.unwrap_or_else(|| "{id}".to_owned());
        if template.matches("{id}").count() != 1
            || template.contains(['?', '#'])
            || template.split('/').any(|part| part == "..")
        {
            return fail(
                "resolver.http.vod_module.drm.path_template must contain {id} once, with no query, fragment or `..`",
            );
        }
        let extra_args = self
            .extra_args
            .map(|args| args.trim_start_matches('?').to_owned())
            .filter(|args| !args.is_empty());
        if extra_args
            .as_deref()
            .is_some_and(|args| args.contains(['#', ' ']) || args.chars().any(char::is_control))
        {
            return fail("resolver.http.vod_module.drm.extra_args must be a plain query string");
        }
        if self
            .host_header
            .as_deref()
            .is_some_and(|host| host.is_empty() || !host.bytes().all(|b| b.is_ascii_graphic()))
        {
            return fail(
                "resolver.http.vod_module.drm.host_header must be a visible ASCII host name",
            );
        }
        if self.max_response_bytes == 0 {
            return fail(
                "resolver.http.vod_module.drm.max_response_bytes must be greater than zero",
            );
        }
        Ok(VodDrmConfig {
            path_template: template,
            extra_args,
            host_header: self.host_header,
            clear_lead_segments: self.clear_lead_segments,
            max_response_bytes: self.max_response_bytes,
        })
    }
}

/// `[resolver.http.vod_module]`, as written.
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawVodModule {
    path_template: Option<String>,
    extra_args: Option<String>,
    host_header: Option<String>,
    file_root: Option<String>,
    remote_base_url: Option<String>,
    health_url: Option<String>,
    ignore: Vec<String>,
    /// Present means the key service is asked for every asset (TDD 0018).
    drm: Option<RawVodDrm>,
}

/// The settings of the nginx-vod-module dialect, checked.
#[derive(Debug, Clone)]
pub(crate) struct VodModuleConfig {
    /// After `base_url`; `{id}` is the asset ID, percent-encoded.
    pub(crate) path_template: String,
    /// A static query string (nginx-vod-module's `vod_upstream_extra_args`).
    pub(crate) extra_args: Option<String>,
    pub(crate) host_header: Option<String>,
    /// Prefix of a mapping's file path, removed to make it relative to `storage.media_root`.
    pub(crate) file_root: Option<String>,
    /// Where `sourceType: http` paths live.
    pub(crate) remote_base_url: Option<String>,
    /// Absent means the mapping service is assumed healthy.
    pub(crate) health_url: Option<String>,
    /// JSON paths or field names accepted and dropped instead of refused.
    pub(crate) ignore: Vec<String>,
    /// The key service of a deployment that encrypts with nginx-vod-module's DRM; `None` serves
    /// every asset clear.
    pub(crate) drm: Option<VodDrmConfig>,
    /// Set by `[url_compat]`: an asset ID made from a file URI (`compat_id`) is turned back into
    /// that URI, slashes and all, in the mapping request's path.
    pub(crate) compat_ids: bool,
}

impl Default for VodModuleConfig {
    fn default() -> Self {
        Self {
            path_template: "{id}".to_owned(),
            extra_args: None,
            host_header: None,
            file_root: None,
            remote_base_url: None,
            health_url: None,
            ignore: Vec::new(),
            drm: None,
            compat_ids: false,
        }
    }
}

impl RawVodModule {
    fn validate(self) -> Result<VodModuleConfig> {
        let fail = |message: &str| Err(Error::Configuration(message.to_owned()));
        let template = self.path_template.unwrap_or_else(|| "{id}".to_owned());
        if template.matches("{id}").count() != 1
            || template.contains(['?', '#'])
            || template.split('/').any(|part| part == "..")
        {
            return fail(
                "resolver.http.vod_module.path_template must contain {id} once, with no query, fragment or `..`",
            );
        }
        let extra_args = self
            .extra_args
            .map(|args| args.trim_start_matches('?').to_owned())
            .filter(|args| !args.is_empty());
        if extra_args
            .as_deref()
            .is_some_and(|args| args.contains(['#', ' ']) || args.chars().any(char::is_control))
        {
            return fail("resolver.http.vod_module.extra_args must be a plain query string");
        }
        if self
            .host_header
            .as_deref()
            .is_some_and(|host| host.is_empty() || !host.bytes().all(|b| b.is_ascii_graphic()))
        {
            return fail("resolver.http.vod_module.host_header must be a visible ASCII host name");
        }
        let is_url = |url: &str| url.starts_with("https://") || url.starts_with("http://");
        if self
            .remote_base_url
            .as_deref()
            .is_some_and(|url| !is_url(url) || url.contains(['?', '#']))
        {
            return fail(
                "resolver.http.vod_module.remote_base_url must be an http(s) URL without a query",
            );
        }
        let health_url = self.health_url.filter(|url| !url.is_empty());
        if health_url.as_deref().is_some_and(|url| !is_url(url)) {
            return fail("resolver.http.vod_module.health_url must be an http(s) URL");
        }
        if self
            .file_root
            .as_deref()
            .is_some_and(|root| !root.starts_with('/') || root.contains('\0'))
        {
            return fail("resolver.http.vod_module.file_root must be an absolute path");
        }
        Ok(VodModuleConfig {
            path_template: template,
            extra_args,
            host_header: self.host_header,
            file_root: self.file_root,
            remote_base_url: self.remote_base_url,
            health_url,
            ignore: self.ignore,
            drm: self.drm.map(RawVodDrm::validate).transpose()?,
            compat_ids: false,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawMapper {
    dialect: Dialect,
    vod_module: Option<RawVodModule>,
    base_url: String,
    bearer_token_env: Option<String>,
    /// A file holding the token, re-read every `bearer_token_reload_ms` so a rotated token takes
    /// effect without a restart. Exclusive with `bearer_token_env`.
    bearer_token_file: Option<PathBuf>,
    bearer_token_reload_ms: u64,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    max_retries: u32,
    max_response_bytes: usize,
    default_ttl_ms: u64,
    min_ttl_ms: u64,
    max_ttl_ms: u64,
    negative_ttl_ms: u64,
    error_ttl_ms: u64,
    stale_if_error_ms: u64,
    refresh_margin_ms: u64,
    readiness_probe_interval_ms: u64,
    allow_insecure_mapper: bool,
}

impl Default for RawMapper {
    fn default() -> Self {
        Self {
            dialect: Dialect::Native,
            vod_module: None,
            base_url: String::new(),
            bearer_token_env: None,
            bearer_token_file: None,
            bearer_token_reload_ms: 5000,
            connect_timeout_ms: 500,
            request_timeout_ms: 2000,
            max_retries: 2,
            max_response_bytes: 16 * 1024,
            default_ttl_ms: 300_000,
            min_ttl_ms: 5000,
            max_ttl_ms: 3_600_000,
            negative_ttl_ms: 5000,
            error_ttl_ms: 2000,
            stale_if_error_ms: 60_000,
            refresh_margin_ms: 30_000,
            readiness_probe_interval_ms: 0,
            allow_insecure_mapper: false,
        }
    }
}

/// The validated mapper client settings.
#[derive(Debug, Clone)]
pub(crate) struct MapperConfig {
    pub(crate) dialect: Dialect,
    pub(crate) vod_module: Box<VodModuleConfig>,
    pub(crate) base_url: String,
    pub(crate) bearer_token: Option<Secret>,
    /// Where `bearer_token` came from, when it is a file that is re-read.
    pub(crate) bearer_token_file: Option<PathBuf>,
    /// Zero reads the file once, at startup.
    pub(crate) bearer_token_reload_ms: u64,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_retries: u32,
    pub(crate) max_response_bytes: usize,
    pub(crate) default_ttl_ms: u64,
    pub(crate) min_ttl_ms: u64,
    pub(crate) max_ttl_ms: u64,
    pub(crate) negative_ttl_ms: u64,
    pub(crate) error_ttl_ms: u64,
    pub(crate) stale_if_error_ms: u64,
    /// How long before a location's `expires_at` the answer is refreshed, so a signed URL is
    /// replaced while the old one still works. Refresh never happens before half the remaining
    /// lifetime has passed, so very short-lived URLs do not thrash.
    pub(crate) refresh_margin_ms: u64,
    /// Zero disables the background reachability probe.
    pub(crate) readiness_probe_interval_ms: u64,
}

impl RawMapper {
    pub(super) fn validate(
        self,
        config_directory: &Path,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<MapperConfig> {
        let fail = |message: &str| Err(Error::Configuration(message.to_owned()));
        let url = self.base_url.trim_end_matches('/');
        let scheme_ok = url.starts_with("https://")
            || (self.allow_insecure_mapper && url.starts_with("http://"));
        if url.is_empty() || !scheme_ok || url.contains(['?', '#']) {
            return fail(
                "resolver.http.base_url must be an https URL without a query (http requires allow_insecure_mapper)",
            );
        }
        if url.split_once("://").is_some_and(|(_, rest)| {
            rest.split('/')
                .next()
                .is_some_and(|authority| authority.contains('@'))
        }) {
            return fail("resolver.http.base_url must not contain credentials");
        }
        if self.connect_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.max_response_bytes == 0
            || self.default_ttl_ms == 0
            || self.min_ttl_ms == 0
            || self.max_ttl_ms == 0
            || self.negative_ttl_ms == 0
            || self.error_ttl_ms == 0
            || self.refresh_margin_ms == 0
        {
            return fail("resolver.http timeouts, sizes, and TTLs must be greater than zero");
        }
        if !(self.min_ttl_ms <= self.default_ttl_ms && self.default_ttl_ms <= self.max_ttl_ms) {
            return fail("resolver.http requires min_ttl_ms <= default_ttl_ms <= max_ttl_ms");
        }
        let (bearer_token, bearer_token_file) =
            match (self.bearer_token_env, self.bearer_token_file) {
                (None, None) => (None, None),
                (Some(name), None) => {
                    let value = environment(&name)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            Error::Configuration(format!(
                                "resolver.http.bearer_token_env names `{name}`, which is not set"
                            ))
                        })?;
                    (Some(Secret(value)), None)
                }
                (None, Some(path)) => {
                    let path = if path.is_absolute() {
                        path
                    } else {
                        config_directory.join(path)
                    };
                    let token = read_token_file(&path).map_err(|reason| {
                        Error::Configuration(format!(
                            "resolver.http.bearer_token_file {}: {reason}",
                            path.display()
                        ))
                    })?;
                    (Some(token), Some(path))
                }
                (Some(_), Some(_)) => {
                    return fail(
                        "resolver.http takes at most one of bearer_token_env and bearer_token_file",
                    );
                }
            };
        if self.dialect == Dialect::Native && self.vod_module.is_some() {
            return fail("resolver.http.vod_module needs dialect = \"vod-module\"");
        }
        let vod_module = Box::new(self.vod_module.unwrap_or_default().validate()?);
        Ok(MapperConfig {
            dialect: self.dialect,
            vod_module,
            base_url: url.to_owned(),
            bearer_token,
            bearer_token_file,
            bearer_token_reload_ms: self.bearer_token_reload_ms,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            max_retries: self.max_retries,
            max_response_bytes: self.max_response_bytes,
            default_ttl_ms: self.default_ttl_ms,
            min_ttl_ms: self.min_ttl_ms,
            max_ttl_ms: self.max_ttl_ms,
            negative_ttl_ms: self.negative_ttl_ms,
            error_ttl_ms: self.error_ttl_ms,
            stale_if_error_ms: self.stale_if_error_ms,
            refresh_margin_ms: self.refresh_margin_ms,
            readiness_probe_interval_ms: self.readiness_probe_interval_ms,
        })
    }
}

/// Caches and load scheduling for the asset registry.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RegistryConfig {
    /// Resolutions kept per asset ID.
    pub(crate) max_cached_resolutions: usize,
    /// How long a request waits for a load slot before `503`.
    pub(crate) load_queue_timeout_ms: u64,
    /// With the static resolver, load every asset before serving so a bad file fails startup.
    pub(crate) preload: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            max_cached_resolutions: 10_000,
            load_queue_timeout_ms: 5000,
            preload: true,
        }
    }
}

impl RegistryConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.max_cached_resolutions == 0 || self.load_queue_timeout_ms == 0 {
            return Err(Error::Configuration(
                "registry limits must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Policy and limits for media read from remote HTTP locations.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RemoteMediaConfig {
    /// Hosts a mapper may point at; empty means remote locations are refused.
    pub(crate) allowed_hosts: Vec<String>,
    /// Permit `http://` locations (development only).
    pub(crate) allow_insecure_http: bool,
    /// Permit locations that resolve to loopback, private, or link-local addresses.
    pub(crate) allow_private_addresses: bool,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_retries: u32,
    /// Simultaneous ranged requests to remote origins.
    pub(crate) max_inflight_reads: usize,
}

impl Default for RemoteMediaConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            allow_insecure_http: false,
            allow_private_addresses: false,
            connect_timeout_ms: 1000,
            request_timeout_ms: 10_000,
            max_retries: 2,
            max_inflight_reads: 64,
        }
    }
}

impl RemoteMediaConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.connect_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.max_inflight_reads == 0
        {
            return Err(Error::Configuration(
                "remote_media timeouts and limits must be greater than zero".to_owned(),
            ));
        }
        if self
            .allowed_hosts
            .iter()
            .any(|host| host.is_empty() || host.contains(['/', ':', '@', ' ']))
        {
            return Err(Error::Configuration(
                "remote_media.allowed_hosts entries must be bare host names".to_owned(),
            ));
        }
        Ok(())
    }
}
