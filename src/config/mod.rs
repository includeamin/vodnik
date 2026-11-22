use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

mod authorization;
mod cors;
mod limits;
mod logging;
mod resolver;
mod tls;
mod url_compat;

pub(crate) use authorization::AuthorizationSettings;
#[cfg(test)]
pub(crate) use authorization::Transports;
pub(crate) use cors::CorsConfig;
pub(crate) use limits::LimitsConfig;
pub(crate) use logging::{LogFormat, LoggingConfig};
pub(crate) use resolver::{
    Dialect, MapperConfig, RegistryConfig, RemoteMediaConfig, ResolverSettings, Secret,
    VodDrmConfig, read_token_file,
};
pub(crate) use tls::TlsConfig;
#[cfg(test)]
pub(crate) use tls::for_test as tls_for_test;
pub(crate) use url_compat::UrlCompatSettings;

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) shutdown_delay_ms: u64,
    pub(crate) shutdown_grace_ms: u64,
    /// `None` serves plain HTTP, as today. See `docs/operations.md#tls`.
    pub(crate) tls: Option<TlsConfig>,
    pub(crate) cors: CorsConfig,
    /// Accept the entry URLs of an nginx-vod-module deployment (TDD 0019); `None` does not.
    pub(crate) url_compat: Option<UrlCompatSettings>,
    pub(crate) segment_duration_ms: u64,
    /// Signed playback tokens (TDD 0007); `None` checks nothing.
    pub(crate) authorization: Option<AuthorizationSettings>,
    /// Carry the default audio track inside HLS video segments (TDD 0011).
    pub(crate) hls_mux_audio: bool,
    pub(crate) assets: BTreeMap<String, PathBuf>,
    pub(crate) media_root: PathBuf,
    pub(crate) resolver: ResolverSettings,
    pub(crate) registry: RegistryConfig,
    pub(crate) remote_media: RemoteMediaConfig,
    pub(crate) logging: LoggingConfig,
    pub(crate) limits: LimitsConfig,
}

impl RawConfig {
    fn validate_nonzero(&self) -> Result<()> {
        if self.packaging.segment_duration_ms == 0 {
            return Err(Error::Configuration(
                "packaging.segment_duration_ms must be greater than zero".to_owned(),
            ));
        }
        if self.logging.buffer_capacity == 0 {
            return Err(Error::Configuration(
                "logging.buffer_capacity must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Config {
    /// A static-catalog configuration assembled in code, for tests and benchmarks. Assets are
    /// canonical absolute paths beneath `media_root`.
    pub(crate) fn for_catalog(
        media_root: PathBuf,
        assets: BTreeMap<String, PathBuf>,
        segment_duration_ms: u64,
        limits: LimitsConfig,
    ) -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            shutdown_delay_ms: 0,
            shutdown_grace_ms: 1000,
            tls: None,
            cors: CorsConfig::default(),
            url_compat: None,
            segment_duration_ms,
            authorization: None,
            hls_mux_audio: default_hls_mux_audio(),
            assets,
            media_root,
            resolver: ResolverSettings::Static,
            registry: RegistryConfig::default(),
            remote_media: RemoteMediaConfig::default(),
            logging: LoggingConfig::default(),
            limits,
        }
    }

    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)?;
        let config_directory = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&contents, config_directory)
    }

    fn parse(contents: &str, config_directory: &Path) -> Result<Self> {
        Self::parse_with(contents, config_directory, &|name| std::env::var(name).ok())
    }

    /// Parses with an injectable environment so secrets can be tested without touching the
    /// process environment.
    fn parse_with(
        contents: &str,
        config_directory: &Path,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents)?;
        raw.validate_nonzero()?;
        raw.limits.validate()?;
        raw.server.validate()?;
        let tls = raw
            .server
            .tls
            .as_ref()
            .map(|raw_tls| raw_tls.validate(config_directory))
            .transpose()?;
        raw.cors.validate()?;
        raw.registry.validate()?;
        raw.remote_media.validate()?;
        let authorization = raw
            .authorization
            .map(|raw| raw.validate(config_directory, environment))
            .transpose()?;
        if raw.assets.len() > raw.limits.max_assets {
            return Err(Error::Configuration(format!(
                "asset count exceeds configured limit {}",
                raw.limits.max_assets
            )));
        }

        let media_root = if raw.storage.media_root.is_absolute() {
            raw.storage.media_root
        } else {
            config_directory.join(raw.storage.media_root)
        }
        .canonicalize()?;
        if !media_root.is_dir() {
            return Err(Error::Configuration(
                "storage.media_root must be a directory".to_owned(),
            ));
        }

        let mut assets = BTreeMap::new();
        for (asset_id, asset) in raw.assets {
            validate_asset_id(&asset_id)?;
            if asset.path.is_absolute() {
                return Err(Error::Configuration(format!(
                    "asset `{asset_id}` path must be relative to storage.media_root"
                )));
            }
            let source = media_root.join(asset.path).canonicalize()?;
            if !source.starts_with(&media_root) || !source.is_file() {
                return Err(Error::Configuration(format!(
                    "asset `{asset_id}` must resolve to a file beneath storage.media_root"
                )));
            }
            assets.insert(asset_id, source);
        }
        let resolver = match (raw.resolver.kind, raw.resolver.http) {
            (resolver::ResolverKind::Static, None) => {
                if assets.is_empty() {
                    return Err(Error::Configuration(
                        "at least one asset must be configured".to_owned(),
                    ));
                }
                ResolverSettings::Static
            }
            (resolver::ResolverKind::Static, Some(_)) => {
                return Err(Error::Configuration(
                    "resolver.http requires resolver.type = \"http\"".to_owned(),
                ));
            }
            (resolver::ResolverKind::Http, None) => {
                return Err(Error::Configuration(
                    "resolver.type = \"http\" requires a [resolver.http] table".to_owned(),
                ));
            }
            (resolver::ResolverKind::Http, Some(mapper)) => {
                if !assets.is_empty() {
                    return Err(Error::Configuration(
                        "[assets] cannot be combined with resolver.type = \"http\"".to_owned(),
                    ));
                }
                ResolverSettings::Http(mapper.validate(config_directory, environment)?)
            }
        };
        let url_compat = raw.url_compat.validate()?;
        let resolver = apply_url_compat(resolver, url_compat.as_ref())?;

        Ok(Self {
            listen: raw.server.listen,
            shutdown_delay_ms: raw.server.shutdown_delay_ms,
            shutdown_grace_ms: raw.server.shutdown_grace_ms,
            tls,
            cors: raw.cors,
            url_compat,
            segment_duration_ms: raw.packaging.segment_duration_ms,
            authorization,
            hls_mux_audio: raw.packaging.hls_mux_audio,
            assets,
            media_root,
            resolver,
            registry: raw.registry,
            remote_media: raw.remote_media,
            logging: raw.logging,
            limits: raw.limits,
        })
    }
}

/// Lets the mapper client turn the IDs of file URIs back into URIs, which only the `vod-module`
/// dialect can pass on (TDD 0019). It is an error to enable the feature with anything else.
fn apply_url_compat(
    resolver: ResolverSettings,
    url_compat: Option<&UrlCompatSettings>,
) -> Result<ResolverSettings> {
    match (resolver, url_compat) {
        (ResolverSettings::Http(mut mapper), Some(_)) if mapper.dialect == Dialect::VodModule => {
            mapper.vod_module.compat_ids = true;
            Ok(ResolverSettings::Http(mapper))
        }
        (_, Some(_)) => Err(Error::Configuration(
            "url_compat needs resolver.type = \"http\" with dialect = \"vod-module\"".to_owned(),
        )),
        (resolver, None) => Ok(resolver),
    }
}

/// Whether `asset_id` is 1 to 128 ASCII letters, digits, `-`, or `_`.
pub(crate) fn is_valid_asset_id(asset_id: &str) -> bool {
    !asset_id.is_empty()
        && asset_id.len() <= 128
        && asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_asset_id(asset_id: &str) -> Result<()> {
    let valid = !asset_id.is_empty()
        && asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(Error::Configuration(format!(
            "asset ID `{asset_id}` must contain only ASCII letters, digits, '-' or '_'"
        )))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: ServerConfig,
    storage: StorageConfig,
    #[serde(default)]
    packaging: PackagingConfig,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    limits: LimitsConfig,
    #[serde(default)]
    cors: CorsConfig,
    #[serde(default)]
    url_compat: url_compat::RawUrlCompat,
    #[serde(default)]
    resolver: resolver::RawResolver,
    #[serde(default)]
    registry: RegistryConfig,
    #[serde(default)]
    remote_media: RemoteMediaConfig,
    #[serde(default)]
    authorization: Option<authorization::RawAuthorization>,
    #[serde(default)]
    assets: BTreeMap<String, AssetConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    listen: SocketAddr,
    #[serde(default)]
    shutdown_delay_ms: u64,
    #[serde(default = "default_shutdown_grace_ms")]
    shutdown_grace_ms: u64,
    #[serde(default)]
    tls: Option<tls::RawTls>,
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        if self.shutdown_grace_ms == 0 {
            return Err(Error::Configuration(
                "server.shutdown_grace_ms must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

const fn default_shutdown_grace_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageConfig {
    media_root: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagingConfig {
    #[serde(default = "default_segment_duration_ms")]
    segment_duration_ms: u64,
    #[serde(default = "default_hls_mux_audio")]
    hls_mux_audio: bool,
}

impl Default for PackagingConfig {
    fn default() -> Self {
        Self {
            segment_duration_ms: default_segment_duration_ms(),
            hls_mux_audio: default_hls_mux_audio(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetConfig {
    path: PathBuf,
}

const fn default_segment_duration_ms() -> u64 {
    6000
}

/// On: a single-file asset's HLS video segments carry its default audio, so a viewer needs two
/// playlists and one segment stream rather than three and two (TDD 0011).
const fn default_hls_mux_audio() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::logging::LogLevel;
    use super::*;

    fn fixture_directory() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn resolves_an_asset_beneath_the_media_root() {
        let config = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"

                [storage]
                media_root = "."

                [packaging]
                segment_duration_ms = 1000

                [assets.sample]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect("configuration should be valid");

        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.segment_duration_ms, 1000);
        assert!(config.hls_mux_audio, "muxing is on unless turned off");
        assert_eq!(config.assets.len(), 1);
        assert_eq!(config.logging.level, LogLevel::Info);
        assert_eq!(config.logging.format, LogFormat::Json);
        assert_eq!(config.limits.max_tracks, 8);
        assert!(config.assets["sample"].ends_with("tests/fixtures/h264-aac.mp4"));
    }

    #[test]
    fn hls_mux_audio_can_be_turned_off() {
        let config = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"

                [storage]
                media_root = "."

                [packaging]
                hls_mux_audio = false

                [assets.sample]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect("configuration should be valid");

        assert!(!config.hls_mux_audio);
    }

    #[test]
    fn fragment_discovery_settings_have_safe_defaults_and_can_be_set() {
        let parse = |limits: &str| {
            Config::parse(
                &format!(
                    "[server]\nlisten = \"127.0.0.1:8080\"\n[storage]\nmedia_root = \".\"\n\
                     [assets.sample]\npath = \"h264-aac.mp4\"\n[limits]\n{limits}\n"
                ),
                &fixture_directory(),
            )
        };

        let defaults = parse("").expect("defaults should be valid").limits;
        assert_eq!(defaults.max_fragments, 20_000);
        assert_eq!(defaults.metadata_concurrency, 16);
        assert!(
            !defaults.tolerate_truncated_tail,
            "cut files are refused unless asked"
        );

        let set =
            parse("max_fragments = 500\nmetadata_concurrency = 4\ntolerate_truncated_tail = true")
                .expect("explicit values should be valid")
                .limits;
        assert_eq!(
            (
                set.max_fragments,
                set.metadata_concurrency,
                set.tolerate_truncated_tail
            ),
            (500, 4, true)
        );

        assert!(parse("metadata_concurrency = 0").is_err());
        assert!(parse("max_fragments = 0").is_err());
    }

    #[test]
    fn rejects_asset_ids_that_are_not_url_safe() {
        let error = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [assets."../sample"]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect_err("unsafe asset ID should fail");

        assert!(error.to_string().contains("asset ID"));
    }

    #[test]
    fn rejects_asset_paths_outside_the_media_root() {
        let error = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [assets.sample]
                path = "../../Cargo.toml"
            "#,
            &fixture_directory(),
        )
        .expect_err("escaping path should fail");

        assert!(error.to_string().contains("beneath storage.media_root"));
    }

    #[test]
    fn parses_logging_configuration() {
        let config = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [logging]
                level = "debug"
                format = "compact"
                buffer_capacity = 1024
                [assets.sample]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect("logging configuration should be valid");

        assert_eq!(config.logging.level, LogLevel::Debug);
        assert_eq!(config.logging.format, LogFormat::Compact);
        assert_eq!(config.logging.buffer_capacity, 1024);
    }

    fn parse_with(extra: &str) -> Result<Config> {
        Config::parse(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    {extra}
                    [assets.sample]
                    path = "h264-aac.mp4"
                "#
            ),
            &fixture_directory(),
        )
    }

    fn authorization_with(extra: &str, env: &[(&str, &str)]) -> Result<Config> {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::parse_with(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    [authorization]
                    {extra}
                    [assets.sample]
                    path = "h264-aac.mp4"
                "#
            ),
            &fixture_directory(),
            &|name| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
        )
    }

    const GOOD_SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn authorization_is_off_unless_configured() {
        assert!(parse_with("").unwrap().authorization.is_none());
    }

    #[test]
    fn authorization_takes_its_secret_from_the_environment_with_defaults() {
        let config = authorization_with(
            "algorithm = \"HS256\"\nkey_env = \"TOKEN_KEY\"",
            &[("TOKEN_KEY", GOOD_SECRET)],
        )
        .unwrap();
        let settings = config.authorization.unwrap();
        assert_eq!(
            settings.transports,
            Transports {
                header: true,
                cookie: true,
                query: true
            }
        );
        assert_eq!(settings.query_parameter, "auth");
        assert_eq!(settings.cookie_name, "vodnik_auth");
        assert!(!format!("{settings:?}").contains(GOOD_SECRET));
    }

    #[test]
    fn authorization_rejects_what_it_cannot_use() {
        let env = [("TOKEN_KEY", GOOD_SECRET)];
        let base = "algorithm = \"HS256\"\nkey_env = \"TOKEN_KEY\"\n";
        for (extra, env, needle) in [
            (
                "algorithm = \"RS256\"\nkey_env = \"TOKEN_KEY\"".to_owned(),
                &env[..],
                "RS256",
            ),
            (base.replace("TOKEN_KEY", "MISSING"), &env[..], "MISSING"),
            ("algorithm = \"HS256\"".to_owned(), &env[..], "exactly one"),
            (
                format!("{base}key_file = \"k.pem\""),
                &env[..],
                "exactly one",
            ),
            (base.to_owned(), &[("TOKEN_KEY", "short")][..], "key"),
            (format!("{base}transports = []"), &env[..], "at least one"),
            (
                format!("{base}transports = [\"header\", \"header\"]"),
                &env[..],
                "twice",
            ),
            (format!("{base}transports = [\"body\"]"), &env[..], "body"),
            (
                format!("{base}clock_skew_secs = 301"),
                &env[..],
                "clock_skew_secs",
            ),
            (
                format!("{base}max_token_bytes = 8"),
                &env[..],
                "max_token_bytes",
            ),
            (
                format!("{base}query_parameter = \"a b\""),
                &env[..],
                "query_parameter",
            ),
            (format!("{base}cookie_name = \"\""), &env[..], "cookie_name"),
            (format!("{base}surprise = 1"), &env[..], "surprise"),
        ] {
            let error = authorization_with(&extra, env).unwrap_err().to_string();
            assert!(error.contains(needle), "{extra}: {error}");
            assert!(!error.contains(GOOD_SECRET), "{error}");
        }
    }

    #[test]
    fn cors_defaults_allow_any_origin_and_expose_range_headers() {
        let config = parse_with("").expect("defaults should be valid");

        assert!(config.cors.enabled);
        assert_eq!(config.cors.allowed_origins, ["*"]);
        assert!(
            config
                .cors
                .exposed_headers
                .iter()
                .any(|name| name == "content-range")
        );
        assert_eq!(config.shutdown_grace_ms, 30_000);
    }

    #[test]
    fn parses_explicit_cors_policy() {
        let config = parse_with(
            r#"
            [cors]
            allowed_origins = ["https://player.example.com", "http://localhost:5173"]
            allowed_headers = ["range"]
            allow_credentials = true
            max_age_seconds = 600
            "#,
        )
        .expect("explicit CORS policy should be valid");

        assert_eq!(config.cors.allowed_origins.len(), 2);
        assert!(config.cors.allow_credentials);
        assert_eq!(config.cors.max_age_seconds, 600);
    }

    #[test]
    fn rejects_invalid_cors_policies() {
        for extra in [
            "[cors]\nallowed_origins = []",
            "[cors]\nallowed_origins = [\"*\", \"https://a.example\"]",
            "[cors]\nallow_credentials = true",
            "[cors]\nallowed_origins = [\"https://a.example/path\"]",
            "[cors]\nallowed_origins = [\"a.example\"]",
        ] {
            parse_with(extra).expect_err(extra);
        }
    }

    #[test]
    fn disabled_cors_skips_validation() {
        parse_with("[cors]\nenabled = false\nallowed_origins = []")
            .expect("disabled CORS should not be validated");
    }

    #[test]
    fn parses_an_http_resolver_without_assets() {
        let config = parse_with(
            r#"
            [resolver]
            type = "http"
            [resolver.http]
            base_url = "https://mapper.example.net/"
            "#,
        );
        // parse_with adds a static asset; the two forms are mutually exclusive.
        assert!(config.is_err());

        let config = Config::parse_with(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [resolver]
                type = "http"
                [resolver.http]
                base_url = "https://mapper.example.net/"
                bearer_token_env = "VOD_TOKEN"
                [remote_media]
                allowed_hosts = ["origin.example.net"]
            "#,
            &fixture_directory(),
            &|name| (name == "VOD_TOKEN").then(|| "secret-value".to_owned()),
        )
        .expect("http resolver configuration should be valid");

        let ResolverSettings::Http(mapper) = &config.resolver else {
            panic!("expected the http resolver");
        };
        assert_eq!(mapper.base_url, "https://mapper.example.net");
        assert_eq!(
            mapper.bearer_token.as_ref().unwrap().expose(),
            "secret-value"
        );
        assert!(
            !format!("{mapper:?}").contains("secret-value"),
            "tokens must be redacted"
        );
        assert_eq!(config.assets.len(), 0);
        assert_eq!(config.remote_media.allowed_hosts, ["origin.example.net"]);
    }

    fn mapper_config(extra: &str) -> Result<Config> {
        Config::parse_with(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    [resolver]
                    type = "http"
                    [resolver.http]
                    {extra}
                "#
            ),
            &fixture_directory(),
            &|_| None,
        )
    }

    #[test]
    fn rejects_invalid_mapper_settings() {
        for extra in [
            "",
            "base_url = \"http://mapper.example.net\"",
            "base_url = \"https://user:pw@mapper.example.net\"",
            "base_url = \"https://mapper.example.net?x=1\"",
            "base_url = \"https://m.example.net\"\nrequest_timeout_ms = 0",
            "base_url = \"https://m.example.net\"\nmin_ttl_ms = 999999999",
            "base_url = \"https://m.example.net\"\nbearer_token_env = \"UNSET_VARIABLE\"",
        ] {
            mapper_config(extra).expect_err(extra);
        }
        mapper_config("base_url = \"http://localhost:9\"\nallow_insecure_mapper = true")
            .expect("insecure mapper is allowed when opted in");
    }

    #[test]
    fn the_vod_module_dialect_is_configured_and_checked() {
        let config = mapper_config(
            r#"
            base_url = "https://mapping.internal.example.net"
            dialect = "vod-module"
            [resolver.http.vod_module]
            path_template = "vod/{id}"
            extra_args = "?key=abc"
            host_header = "mapping.example"
            file_root = "/data/media"
            remote_base_url = "https://origin.example.net"
            ignore = ["notifications"]
            "#,
        )
        .expect("a complete dialect section is accepted");
        let ResolverSettings::Http(mapper) = &config.resolver else {
            panic!("expected the http resolver");
        };
        assert_eq!(mapper.dialect, Dialect::VodModule);
        assert_eq!(mapper.vod_module.path_template, "vod/{id}");
        assert_eq!(mapper.vod_module.extra_args.as_deref(), Some("key=abc"));
        assert_eq!(mapper.vod_module.ignore, ["notifications"]);

        let default =
            mapper_config("base_url = \"https://m.example.net\"\ndialect = \"vod-module\"")
                .expect("every dialect setting is optional");
        let ResolverSettings::Http(mapper) = &default.resolver else {
            panic!("expected the http resolver");
        };
        assert_eq!(mapper.vod_module.path_template, "{id}");
    }

    fn compat_config(url_compat: &str, resolver: &str) -> Result<Config> {
        Config::parse_with(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    {url_compat}
                    {resolver}
                "#
            ),
            &fixture_directory(),
            &|_| None,
        )
    }

    const VOD_MODULE_RESOLVER: &str = "[resolver]\ntype = \"http\"\n[resolver.http]\nbase_url = \"https://m.example.net\"\ndialect = \"vod-module\"";

    #[test]
    fn url_compatibility_is_opt_in_and_needs_the_vod_module_dialect() {
        let off = compat_config("", VOD_MODULE_RESOLVER).unwrap();
        assert!(off.url_compat.is_none());
        let disabled = compat_config("[url_compat]\nenabled = false", VOD_MODULE_RESOLVER).unwrap();
        assert!(disabled.url_compat.is_none());

        let on = compat_config(
            "[url_compat]\nenabled = true\npath_prefix = \"/vod/\"",
            VOD_MODULE_RESOLVER,
        )
        .unwrap();
        assert_eq!(on.url_compat.as_ref().unwrap().path_prefix, "/vod");
        let ResolverSettings::Http(mapper) = &on.resolver else {
            panic!("expected the http resolver");
        };
        assert!(
            mapper.vod_module.compat_ids,
            "the mapper client turns the IDs back into URIs"
        );

        // Without the dialect there is nothing to pass a file URI on to.
        for resolver in [
            "",
            "[resolver]\ntype = \"http\"\n[resolver.http]\nbase_url = \"https://m.example.net\"",
        ] {
            compat_config("[url_compat]\nenabled = true", resolver).expect_err(resolver);
        }
    }

    #[test]
    fn a_url_compatibility_prefix_must_be_a_plain_absolute_path() {
        for prefix in [
            "vod", "/a//b", "/a/../b", "/a?x", "/a#x", "/a b", "/%2e", "/a\\\\b", "//",
        ] {
            compat_config(
                &format!("[url_compat]\nenabled = true\npath_prefix = \"{prefix}\""),
                VOD_MODULE_RESOLVER,
            )
            .expect_err(prefix);
        }
        compat_config(
            "[url_compat]\nenabled = true\npath_prefix = \"/a/b\"",
            VOD_MODULE_RESOLVER,
        )
        .expect("a nested prefix is fine");
        compat_config(
            "[url_compat]\nenabled = true\nunknown = 1",
            VOD_MODULE_RESOLVER,
        )
        .expect_err("unknown fields are refused");
    }

    #[test]
    fn the_drm_bridge_is_configured_and_checked() {
        let base = "base_url = \"https://m.example.net\"\ndialect = \"vod-module\"\n[resolver.http.vod_module.drm]\n";
        let config = mapper_config(&format!(
            "{base}path_template = \"drm/{{id}}\"\nextra_args = \"?k=1\"\nhost_header = \"keys.example\"\nclear_lead_segments = 2\nmax_response_bytes = 4096"
        ))
        .expect("a complete drm section is accepted");
        let ResolverSettings::Http(mapper) = &config.resolver else {
            panic!("expected the http resolver");
        };
        let drm = mapper.vod_module.drm.as_ref().expect("drm is configured");
        assert_eq!(drm.path_template, "drm/{id}");
        assert_eq!(drm.extra_args.as_deref(), Some("k=1"));
        assert_eq!(drm.clear_lead_segments, 2);
        assert_eq!(drm.max_response_bytes, 4096);
        // Without the table there is no key service.
        let plain = mapper_config("base_url = \"https://m.example.net\"\ndialect = \"vod-module\"")
            .unwrap();
        let ResolverSettings::Http(mapper) = &plain.resolver else {
            panic!("expected the http resolver");
        };
        assert!(mapper.vod_module.drm.is_none());
        for extra in [
            "path_template = \"drm/none\"",
            "path_template = \"{id}?x=1\"",
            "path_template = \"../{id}\"",
            "extra_args = \"a=1#frag\"",
            "host_header = \"two words\"",
            "max_response_bytes = 0",
            "unknown = 1",
        ] {
            mapper_config(&format!("{base}{extra}")).expect_err(extra);
        }
    }

    #[test]
    fn invalid_vod_module_settings_are_rejected() {
        let base = "base_url = \"https://m.example.net\"\ndialect = \"vod-module\"\n[resolver.http.vod_module]\n";
        for extra in [
            "path_template = \"vod/none\"",
            "path_template = \"{id}/{id}\"",
            "path_template = \"{id}?x=1\"",
            "path_template = \"../{id}\"",
            "extra_args = \"a=1#frag\"",
            "host_header = \"two words\"",
            "file_root = \"relative/path\"",
            "remote_base_url = \"ftp://origin\"",
            "health_url = \"not a url\"",
            "unknown = 1",
        ] {
            mapper_config(&format!("{base}{extra}")).expect_err(extra);
        }
        // The section means nothing without the dialect.
        mapper_config(
            "base_url = \"https://m.example.net\"\n[resolver.http.vod_module]\nfile_root = \"/data\"",
        )
        .expect_err("a vod_module section needs dialect = vod-module");
        mapper_config("base_url = \"https://m.example.net\"\ndialect = \"other\"")
            .expect_err("an unknown dialect");
    }

    #[test]
    fn a_mapper_token_file_is_read_trimmed_and_kept_out_of_debug_output() {
        let directory = crate::testutil::ScratchDir::new("token-file-read");
        fs::write(directory.path().join("token"), "file-secret\n").unwrap();
        let config = Config::parse_with(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[storage]\nmedia_root = \".\"\n[resolver]\ntype = \"http\"\n[resolver.http]\nbase_url = \"https://m.example.net\"\nbearer_token_file = \"token\"\n[remote_media]\nallowed_hosts = [\"origin.example.net\"]",
            directory.path(),
            &|_| None,
        )
        .unwrap();
        let ResolverSettings::Http(mapper) = &config.resolver else {
            panic!("expected the http resolver");
        };
        assert_eq!(
            mapper.bearer_token.as_ref().unwrap().expose(),
            "file-secret"
        );
        assert_eq!(
            mapper.bearer_token_file.as_deref(),
            Some(directory.path().join("token").as_path())
        );
        assert_eq!(mapper.bearer_token_reload_ms, 5000);
        assert!(!format!("{mapper:?}").contains("file-secret"));
    }

    #[test]
    fn a_mapper_token_file_that_cannot_be_used_is_rejected_without_its_contents() {
        let directory = crate::testutil::ScratchDir::new("token-file-bad");
        let path = directory.path().join("token");
        for (contents, needle) in [
            (Some(""), "empty"),
            (Some("\n"), "empty"),
            (Some("two words"), "visible ASCII"),
            (Some("line\nbreak"), "visible ASCII"),
            (None, "cannot be read"),
        ] {
            match contents {
                Some(contents) => fs::write(&path, contents).unwrap(),
                None => {
                    let _ = fs::remove_file(&path);
                }
            }
            let error = read_token_file(&path).unwrap_err();
            assert!(error.contains(needle), "{contents:?}: {error}");
        }
        let error =
            mapper_config("base_url = \"https://m.example.net\"\nbearer_token_file = \"missing\"")
                .unwrap_err()
                .to_string();
        assert!(error.contains("bearer_token_file"), "{error}");
        mapper_config(
            "base_url = \"https://m.example.net\"\nbearer_token_file = \"t\"\nbearer_token_env = \"X\"",
        )
        .expect_err("env and file together");
    }

    #[test]
    fn rejects_inconsistent_resolver_sections() {
        parse_with(
            "[resolver]\ntype = \"static\"\n[resolver.http]\nbase_url = \"https://m.example.net\"",
        )
        .expect_err("http table needs the http type");
        Config::parse_with(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[storage]\nmedia_root = \".\"\n[resolver]\ntype = \"http\"",
            &fixture_directory(),
            &|_| None,
        )
        .expect_err("http type needs its table");
    }

    #[test]
    fn rejects_invalid_remote_media_hosts() {
        parse_with("[remote_media]\nallowed_hosts = [\"https://origin.example.net/path\"]")
            .expect_err("hosts must be bare");
        parse_with("[remote_media]\nmax_inflight_reads = 0").expect_err("limits must be positive");
    }
}
