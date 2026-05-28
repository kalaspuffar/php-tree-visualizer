//! Typed, validated configuration loaded from a single TOML file.
//!
//! Implements `SPECIFICATION.md` §7.3 (the on-disk schema) and the
//! `config` sub-module of §3.1. Defaults and required fields follow the
//! decisions in `openspec/changes/config-loader/design.md`.

mod error;
mod secret;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use error::ConfigError;
pub use secret::SecretString;

const ALLOWED_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];
const ALLOWED_LOG_FORMATS: &[&str] = &["json", "text"];

/// The full collector configuration. Mirrors the section structure of
/// `SPECIFICATION.md` §7.3, plus an `[observability]` section owned
/// by the `collector-observability` capability.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
    pub storage: Storage,
    #[serde(default)]
    pub finalize: Finalize,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub log: Log,
    #[serde(default)]
    pub observability: Observability,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub bind: String,
    #[serde(default = "defaults::max_body_bytes")]
    pub max_body_bytes: u64,
    #[serde(default = "defaults::queue_capacity")]
    pub queue_capacity: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub token: SecretString,
    pub session_salt: SecretString,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub data_dir: PathBuf,
    pub retention_days: u32,
    /// Reference denominator for the disk-usage gauge's
    /// `over_threshold` calculation. Optional: when unset the gauge
    /// still emits `data_dir_bytes` but `over_threshold` is always
    /// `false`. Owned by `collector-observability` semantically;
    /// kept under `[storage]` because that's the operator-facing
    /// place to declare "I gave the data dir this much space."
    #[serde(default)]
    pub disk_capacity_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finalize {
    #[serde(default = "defaults::idle_seconds")]
    pub idle_seconds: u32,
    #[serde(default = "defaults::tick_seconds")]
    pub tick_seconds: u32,
    /// Hard cap (seconds) before a trace whose `pending_calls` is
    /// non-empty is force-finalised. Under out-of-order delivery the
    /// presence of pending rows means more batches are expected;
    /// finalising on `idle_seconds` alone destroys the backlog and
    /// produces spurious DQ anomalies (see the
    /// `finalize-defers-on-pending` change). The hard cap is the
    /// safety net so a trace whose resolvers truly never arrive
    /// still finalises and its residual becomes DQ.
    #[serde(default = "defaults::max_pending_seconds")]
    pub max_pending_seconds: u32,
}

impl Default for Finalize {
    fn default() -> Self {
        Self {
            idle_seconds: defaults::idle_seconds(),
            tick_seconds: defaults::tick_seconds(),
            max_pending_seconds: defaults::max_pending_seconds(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    #[serde(default = "defaults::tick_minutes")]
    pub tick_minutes: u32,
    /// Test-only override that, when present, overrides
    /// `tick_minutes` for the purpose of computing the retention
    /// loop's tick interval. Production configs leave this unset
    /// (so the loop ticks every `tick_minutes` minutes); the
    /// integration suite sets it to `1` so a test can observe the
    /// sweeper inside a few seconds. Documented intentionally
    /// absent from `etc/collector.toml.example`.
    #[serde(default)]
    pub tick_seconds: Option<u32>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            tick_minutes: defaults::tick_minutes(),
            tick_seconds: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    #[serde(default = "defaults::log_level")]
    pub level: String,
    #[serde(default = "defaults::log_format")]
    pub format: String,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: defaults::log_level(),
            format: defaults::log_format(),
        }
    }
}

/// Configuration for the `collector-observability` capability — the
/// periodic disk-usage gauge. Logging itself is configured by
/// `[log]`; this section is for the gauge task only.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observability {
    /// How often the gauge fires, in seconds. Default 1 hour.
    #[serde(default = "defaults::disk_usage_tick_seconds")]
    pub disk_usage_tick_seconds: u64,
    /// Threshold percentage. The gauge emits at `warn` level when
    /// `data_dir_bytes >= storage.disk_capacity_bytes * warn_pct%`.
    /// Default 80.
    #[serde(default = "defaults::disk_usage_warn_pct")]
    pub disk_usage_warn_pct: u8,
    /// Test-only override that, when present, overrides
    /// `disk_usage_tick_seconds` for the purpose of computing the
    /// gauge's tick interval. Documented intentionally absent from
    /// `etc/collector.toml.example`. Mirrors the
    /// `retention.tick_seconds` pattern.
    #[serde(default)]
    pub disk_usage_tick_seconds_test_override: Option<u64>,
}

impl Default for Observability {
    fn default() -> Self {
        Self {
            disk_usage_tick_seconds: defaults::disk_usage_tick_seconds(),
            disk_usage_warn_pct: defaults::disk_usage_warn_pct(),
            disk_usage_tick_seconds_test_override: None,
        }
    }
}

/// Defaults mirror `SPECIFICATION.md` §7.3. Defining them as functions
/// (not `const` values) is required by `#[serde(default = "...")]`.
mod defaults {
    pub(super) fn max_body_bytes() -> u64 {
        67_108_864 // §7.3: 64 MiB
    }
    pub(super) fn queue_capacity() -> u32 {
        256 // §7.3
    }
    pub(super) fn idle_seconds() -> u32 {
        30 // §7.3: "trace marked finalized after this much silence"
    }
    pub(super) fn tick_seconds() -> u32 {
        5 // §7.3
    }
    pub(super) fn max_pending_seconds() -> u32 {
        // 10 minutes — generous enough to cover a real
        // out-of-order delivery; bounded so a trace whose
        // resolvers truly never arrive does eventually
        // force-finalise rather than leak as 'active'.
        600
    }
    pub(super) fn tick_minutes() -> u32 {
        60 // §7.3
    }
    pub(super) fn log_level() -> String {
        "info".to_owned() // §7.3
    }
    pub(super) fn log_format() -> String {
        "json".to_owned() // §7.3: "or 'text'"
    }
    pub(super) fn disk_usage_tick_seconds() -> u64 {
        3_600 // one hour
    }
    pub(super) fn disk_usage_warn_pct() -> u8 {
        80
    }
}

impl Config {
    /// Validate cross-field invariants and value ranges that serde's
    /// derive cannot express on its own. Returns the first failure.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.server.validate()?;
        self.auth.validate()?;
        self.storage.validate()?;
        self.finalize.validate()?;
        self.retention.validate()?;
        self.log.validate()?;
        self.observability.validate()?;
        Ok(())
    }
}

impl Server {
    fn validate(&self) -> Result<(), ConfigError> {
        let addr: SocketAddr = self.bind.parse().map_err(|e| {
            ConfigError::bad_value("server.bind", format!("not a socket address: {e}"))
        })?;
        // §3.4 / AC-3.4.1: the collector must not be reachable from
        // outside the host. The reverse proxy is the only thing that
        // talks to us.
        if !addr.ip().is_loopback() {
            return Err(ConfigError::bad_value(
                "server.bind",
                format!("must be a loopback address (127.0.0.0/8 or ::1) (got {addr})"),
            ));
        }
        if self.max_body_bytes == 0 {
            return Err(ConfigError::bad_value(
                "server.max_body_bytes",
                "must be greater than zero",
            ));
        }
        if self.queue_capacity == 0 {
            return Err(ConfigError::bad_value(
                "server.queue_capacity",
                "must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Auth {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.token.char_count() < 32 {
            return Err(ConfigError::bad_value(
                "auth.token",
                format!(
                    "must be at least 32 characters (got {})",
                    self.token.char_count()
                ),
            ));
        }
        if self.session_salt.char_count() < 32 {
            return Err(ConfigError::bad_value(
                "auth.session_salt",
                format!(
                    "must be at least 32 characters (got {})",
                    self.session_salt.char_count()
                ),
            ));
        }
        if self.token.expose_secret() == self.session_salt.expose_secret() {
            return Err(ConfigError::bad_value(
                "auth.session_salt",
                "must be different from auth.token",
            ));
        }
        Ok(())
    }
}

impl Storage {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.data_dir.is_absolute() {
            return Err(ConfigError::bad_value(
                "storage.data_dir",
                format!("must be an absolute path (got {})", self.data_dir.display()),
            ));
        }
        if self.retention_days == 0 {
            return Err(ConfigError::bad_value(
                "storage.retention_days",
                "must be greater than zero",
            ));
        }
        if matches!(self.disk_capacity_bytes, Some(0)) {
            return Err(ConfigError::bad_value(
                "storage.disk_capacity_bytes",
                "must be greater than zero when set (omit to leave the disk-usage threshold disabled)",
            ));
        }
        Ok(())
    }
}

impl Finalize {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.idle_seconds == 0 {
            return Err(ConfigError::bad_value(
                "finalize.idle_seconds",
                "must be greater than zero",
            ));
        }
        if self.tick_seconds == 0 {
            return Err(ConfigError::bad_value(
                "finalize.tick_seconds",
                "must be greater than zero",
            ));
        }
        if self.max_pending_seconds == 0 {
            return Err(ConfigError::bad_value(
                "finalize.max_pending_seconds",
                "must be greater than zero",
            ));
        }
        // Cross-field: a hard cap shorter than the idle threshold
        // would let pending-bearing traces finalise *earlier* than
        // empty ones, which inverts the intent.
        if self.max_pending_seconds < self.idle_seconds {
            return Err(ConfigError::bad_value(
                "finalize.max_pending_seconds",
                "must be greater than or equal to finalize.idle_seconds (the pending hard cap cannot be shorter than the idle threshold)",
            ));
        }
        Ok(())
    }
}

impl Retention {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.tick_minutes == 0 {
            return Err(ConfigError::bad_value(
                "retention.tick_minutes",
                "must be greater than zero",
            ));
        }
        if matches!(self.tick_seconds, Some(0)) {
            return Err(ConfigError::bad_value(
                "retention.tick_seconds",
                "must be greater than zero when set (omit to use tick_minutes)",
            ));
        }
        Ok(())
    }
}

impl Observability {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.disk_usage_tick_seconds == 0 {
            return Err(ConfigError::bad_value(
                "observability.disk_usage_tick_seconds",
                "must be greater than zero",
            ));
        }
        if !(1..=100).contains(&self.disk_usage_warn_pct) {
            return Err(ConfigError::bad_value(
                "observability.disk_usage_warn_pct",
                format!(
                    "must be in the range 1..=100 (got {})",
                    self.disk_usage_warn_pct
                ),
            ));
        }
        if matches!(self.disk_usage_tick_seconds_test_override, Some(0)) {
            return Err(ConfigError::bad_value(
                "observability.disk_usage_tick_seconds_test_override",
                "must be greater than zero when set (omit to use disk_usage_tick_seconds)",
            ));
        }
        Ok(())
    }
}

impl Log {
    fn validate(&self) -> Result<(), ConfigError> {
        if !ALLOWED_LOG_LEVELS.contains(&self.level.as_str()) {
            return Err(ConfigError::bad_value(
                "log.level",
                format!(
                    "must be one of {:?} (got {:?})",
                    ALLOWED_LOG_LEVELS, self.level
                ),
            ));
        }
        if !ALLOWED_LOG_FORMATS.contains(&self.format.as_str()) {
            return Err(ConfigError::bad_value(
                "log.format",
                format!(
                    "must be one of {:?} (got {:?})",
                    ALLOWED_LOG_FORMATS, self.format
                ),
            ));
        }
        Ok(())
    }
}

/// Read, parse, and validate the TOML configuration at `path`.
pub fn load_from_path(path: &Path) -> Result<Config, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::io(path, e))?;
    let config: Config = toml::from_str(&contents).map_err(|e| ConfigError::parse(path, e))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal TOML config containing only the five required fields,
    /// suitable for exercising the defaults.
    fn minimal_toml() -> String {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
"#
        )
    }

    /// A full TOML config matching the §7.3 shape with secrets that
    /// pass validation.
    fn full_toml() -> String {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        format!(
            r#"
[server]
bind = "127.0.0.1:8088"
max_body_bytes = 67108864
queue_capacity = 256

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[finalize]
idle_seconds = 30
tick_seconds = 5

[retention]
tick_minutes = 60

[log]
level = "info"
format = "json"
"#
        )
    }

    fn parse(s: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(|e| ConfigError::parse(Path::new("/x"), e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    // ---- Happy path ----

    #[test]
    fn full_config_parses_and_validates() {
        let cfg = parse(&full_toml()).expect("full config should validate");
        assert_eq!(cfg.server.bind, "127.0.0.1:8088");
        assert_eq!(cfg.server.max_body_bytes, 67_108_864);
        assert_eq!(cfg.server.queue_capacity, 256);
        assert_eq!(cfg.storage.retention_days, 30);
        assert_eq!(cfg.finalize.idle_seconds, 30);
        assert_eq!(cfg.finalize.tick_seconds, 5);
        assert_eq!(cfg.retention.tick_minutes, 60);
        assert_eq!(cfg.log.level, "info");
        assert_eq!(cfg.log.format, "json");
    }

    // ---- Defaults ----

    #[test]
    fn defaults_apply_when_optional_sections_are_absent() {
        let cfg = parse(&minimal_toml()).expect("minimal config should validate");
        assert_eq!(cfg.server.max_body_bytes, 67_108_864);
        assert_eq!(cfg.server.queue_capacity, 256);
        assert_eq!(cfg.finalize.idle_seconds, 30);
        assert_eq!(cfg.finalize.tick_seconds, 5);
        assert_eq!(cfg.retention.tick_minutes, 60);
        assert_eq!(cfg.log.level, "info");
        assert_eq!(cfg.log.format, "json");
    }

    #[test]
    fn explicit_value_overrides_default() {
        let mut toml = minimal_toml();
        toml.push_str("\n[server]\n");
        // The above appends a second [server] section which TOML rejects;
        // build a fresh string instead to override one field.
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"
queue_capacity = 1024

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
"#
        );
        let cfg = parse(&toml).unwrap();
        assert_eq!(cfg.server.queue_capacity, 1024);
        assert_eq!(cfg.server.max_body_bytes, 67_108_864); // default still applies
    }

    // ---- Required fields ----

    #[test]
    fn missing_required_field_token_is_rejected() {
        let toml = r#"
[server]
bind = "127.0.0.1:8088"

[auth]
session_salt = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[storage]
data_dir = "/x"
retention_days = 1
"#;
        let err = parse(toml).expect_err("missing token must fail");
        let msg = format!("{err}");
        assert!(msg.contains("token"), "{msg}");
    }

    #[test]
    fn missing_required_field_bind_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/x"
retention_days = 1
"#
        );
        let err = parse(&toml).expect_err("missing bind must fail");
        assert!(format!("{err}").contains("bind"));
    }

    // ---- Unknown fields ----

    #[test]
    fn unknown_top_level_table_is_rejected() {
        let mut toml = full_toml();
        toml.push_str("\n[mystery]\nfoo = 1\n");
        let err = parse(&toml).expect_err("unknown table must fail");
        let msg = format!("{err}");
        assert!(msg.to_lowercase().contains("mystery"), "{msg}");
    }

    #[test]
    fn unknown_key_inside_known_table_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"
garbage = "x"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/x"
retention_days = 1
"#
        );
        let err = parse(&toml).expect_err("unknown key must fail");
        assert!(format!("{err}").contains("garbage"));
    }

    // ---- Value validation ----

    fn replace_field(toml: &str, key: &str, value: &str) -> String {
        // Replace `key = …` line; simple enough for our test fixtures.
        toml.lines()
            .map(|line| {
                if line.trim_start().starts_with(&format!("{key} ")) {
                    format!("{key} = {value}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn token_shorter_than_32_chars_is_rejected() {
        let toml = full_toml().replace(&"a".repeat(40), "short");
        let err = parse(&toml).expect_err("short token must fail");
        let msg = format!("{err}");
        assert!(msg.contains("auth.token"));
        assert!(msg.contains("at least 32"));
    }

    #[test]
    fn salt_shorter_than_32_chars_is_rejected() {
        let toml = full_toml().replace(&"b".repeat(40), "short");
        let err = parse(&toml).expect_err("short salt must fail");
        let msg = format!("{err}");
        assert!(msg.contains("auth.session_salt"));
    }

    #[test]
    fn token_equal_to_salt_is_rejected() {
        // Make token == salt by using the same 40-char value for both.
        let same = "x".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{same}"
session_salt = "{same}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
"#
        );
        let err = parse(&toml).expect_err("token == salt must fail");
        let msg = format!("{err}");
        assert!(msg.contains("auth.session_salt"));
        assert!(msg.contains("different"));
    }

    #[test]
    fn unparseable_bind_is_rejected() {
        let toml = replace_field(&full_toml(), "bind", "\"not-a-socket\"");
        let err = parse(&toml).expect_err("bad bind must fail");
        let msg = format!("{err}");
        assert!(msg.contains("server.bind"));
    }

    #[test]
    fn ipv4_loopback_bind_is_accepted() {
        let toml = replace_field(&full_toml(), "bind", "\"127.0.0.1:8088\"");
        parse(&toml).expect("127.0.0.1 should pass");
        let toml = replace_field(&full_toml(), "bind", "\"127.0.0.42:8088\"");
        parse(&toml).expect("127.0.0.42 should pass — whole 127.0.0.0/8 is loopback");
    }

    #[test]
    fn ipv6_loopback_bind_is_accepted() {
        let toml = replace_field(&full_toml(), "bind", "\"[::1]:8088\"");
        parse(&toml).expect("[::1] should pass");
    }

    #[test]
    fn wildcard_ipv4_bind_is_rejected_as_non_loopback() {
        let toml = replace_field(&full_toml(), "bind", "\"0.0.0.0:8088\"");
        let err = parse(&toml).expect_err("wildcard bind must fail");
        let msg = format!("{err}");
        assert!(msg.contains("server.bind"));
        assert!(
            msg.contains("loopback"),
            "expected loopback reason; got: {msg}"
        );
    }

    #[test]
    fn routable_ipv4_bind_is_rejected_as_non_loopback() {
        let toml = replace_field(&full_toml(), "bind", "\"192.168.1.1:8088\"");
        let err = parse(&toml).expect_err("routable bind must fail");
        assert!(format!("{err}").contains("loopback"));
    }

    #[test]
    fn non_loopback_ipv6_bind_is_rejected() {
        let toml = replace_field(&full_toml(), "bind", "\"[2001:db8::1]:8088\"");
        let err = parse(&toml).expect_err("non-loopback ipv6 must fail");
        assert!(format!("{err}").contains("loopback"));
    }

    #[test]
    fn relative_data_dir_is_rejected() {
        let toml = replace_field(&full_toml(), "data_dir", "\"data\"");
        let err = parse(&toml).expect_err("relative path must fail");
        let msg = format!("{err}");
        assert!(msg.contains("storage.data_dir"));
        assert!(msg.contains("absolute"));
    }

    #[test]
    fn zero_retention_days_is_rejected() {
        let toml = replace_field(&full_toml(), "retention_days", "0");
        let err = parse(&toml).expect_err("zero retention must fail");
        assert!(format!("{err}").contains("storage.retention_days"));
    }

    #[test]
    fn zero_idle_seconds_is_rejected() {
        let toml = replace_field(&full_toml(), "idle_seconds", "0");
        let err = parse(&toml).expect_err("zero idle must fail");
        assert!(format!("{err}").contains("finalize.idle_seconds"));
    }

    #[test]
    fn zero_tick_seconds_is_rejected() {
        let toml = replace_field(&full_toml(), "tick_seconds", "0");
        let err = parse(&toml).expect_err("zero tick_seconds must fail");
        assert!(format!("{err}").contains("finalize.tick_seconds"));
    }

    #[test]
    fn max_pending_seconds_defaults_to_600() {
        let cfg = parse(&full_toml()).expect("full config (no max_pending_seconds) must validate");
        assert_eq!(cfg.finalize.max_pending_seconds, 600);
    }

    #[test]
    fn zero_max_pending_seconds_is_rejected() {
        let mut toml = full_toml();
        toml = toml.replace(
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5",
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5\nmax_pending_seconds = 0",
        );
        let err = parse(&toml).expect_err("zero max_pending_seconds must fail");
        assert!(format!("{err}").contains("finalize.max_pending_seconds"));
    }

    #[test]
    fn max_pending_seconds_below_idle_is_rejected() {
        let mut toml = full_toml();
        toml = toml.replace(
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5",
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5\nmax_pending_seconds = 10",
        );
        let err = parse(&toml).expect_err("max_pending_seconds < idle_seconds must fail");
        let msg = format!("{err}");
        assert!(msg.contains("finalize.max_pending_seconds"), "{msg}");
        assert!(msg.contains("idle_seconds"), "{msg}");
    }

    #[test]
    fn max_pending_seconds_equal_to_idle_is_accepted() {
        let mut toml = full_toml();
        toml = toml.replace(
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5",
            "[finalize]\nidle_seconds = 30\ntick_seconds = 5\nmax_pending_seconds = 30",
        );
        let cfg = parse(&toml).expect("== boundary must pass");
        assert_eq!(cfg.finalize.max_pending_seconds, 30);
    }

    #[test]
    fn zero_tick_minutes_is_rejected() {
        let toml = replace_field(&full_toml(), "tick_minutes", "0");
        let err = parse(&toml).expect_err("zero tick_minutes must fail");
        assert!(format!("{err}").contains("retention.tick_minutes"));
    }

    #[test]
    fn retention_tick_seconds_defaults_to_none() {
        let cfg = parse(&full_toml()).expect("full config should validate");
        assert_eq!(
            cfg.retention.tick_seconds, None,
            "tick_seconds is a test-only override; absent by default"
        );
    }

    #[test]
    fn retention_tick_seconds_can_be_set_explicitly() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[retention]
tick_minutes = 60
tick_seconds = 2
"#
        );
        let cfg = parse(&toml).expect("explicit tick_seconds must validate");
        assert_eq!(cfg.retention.tick_minutes, 60);
        assert_eq!(cfg.retention.tick_seconds, Some(2));
    }

    #[test]
    fn zero_retention_tick_seconds_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[retention]
tick_minutes = 60
tick_seconds = 0
"#
        );
        let err = parse(&toml).expect_err("zero tick_seconds must fail");
        let msg = format!("{err}");
        assert!(msg.contains("retention.tick_seconds"), "{msg}");
    }

    #[test]
    fn zero_queue_capacity_is_rejected() {
        let toml = replace_field(&full_toml(), "queue_capacity", "0");
        let err = parse(&toml).expect_err("zero queue_capacity must fail");
        assert!(format!("{err}").contains("server.queue_capacity"));
    }

    #[test]
    fn zero_max_body_bytes_is_rejected() {
        let toml = replace_field(&full_toml(), "max_body_bytes", "0");
        let err = parse(&toml).expect_err("zero max_body_bytes must fail");
        assert!(format!("{err}").contains("server.max_body_bytes"));
    }

    #[test]
    fn unknown_log_level_is_rejected() {
        let toml = replace_field(&full_toml(), "level", "\"warning\"");
        let err = parse(&toml).expect_err("bad log.level must fail");
        let msg = format!("{err}");
        assert!(msg.contains("log.level"));
        assert!(msg.contains("warn")); // mentions allowed values
    }

    // ---- Observability section ----

    #[test]
    fn observability_defaults_apply_when_section_is_absent() {
        let cfg = parse(&minimal_toml()).expect("minimal config should validate");
        assert_eq!(cfg.observability.disk_usage_tick_seconds, 3_600);
        assert_eq!(cfg.observability.disk_usage_warn_pct, 80);
        assert_eq!(
            cfg.observability.disk_usage_tick_seconds_test_override,
            None
        );
        assert_eq!(cfg.storage.disk_capacity_bytes, None);
    }

    #[test]
    fn observability_explicit_values_round_trip() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
disk_capacity_bytes = 1073741824

[observability]
disk_usage_tick_seconds = 60
disk_usage_warn_pct = 50
disk_usage_tick_seconds_test_override = 2
"#
        );
        let cfg = parse(&toml).expect("explicit observability section validates");
        assert_eq!(cfg.observability.disk_usage_tick_seconds, 60);
        assert_eq!(cfg.observability.disk_usage_warn_pct, 50);
        assert_eq!(
            cfg.observability.disk_usage_tick_seconds_test_override,
            Some(2)
        );
        assert_eq!(cfg.storage.disk_capacity_bytes, Some(1_073_741_824));
    }

    #[test]
    fn zero_disk_usage_tick_seconds_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[observability]
disk_usage_tick_seconds = 0
"#
        );
        let err = parse(&toml).expect_err("zero disk_usage_tick_seconds must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("observability.disk_usage_tick_seconds"),
            "{msg}"
        );
    }

    #[test]
    fn disk_usage_warn_pct_out_of_range_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[observability]
disk_usage_warn_pct = 101
"#
        );
        let err = parse(&toml).expect_err("disk_usage_warn_pct = 101 must fail");
        let msg = format!("{err}");
        assert!(msg.contains("observability.disk_usage_warn_pct"), "{msg}");
        assert!(msg.contains("1..=100"), "{msg}");
    }

    #[test]
    fn zero_disk_usage_warn_pct_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[observability]
disk_usage_warn_pct = 0
"#
        );
        let err = parse(&toml).expect_err("disk_usage_warn_pct = 0 must fail");
        assert!(format!("{err}").contains("observability.disk_usage_warn_pct"));
    }

    #[test]
    fn zero_disk_usage_test_override_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30

[observability]
disk_usage_tick_seconds_test_override = 0
"#
        );
        let err = parse(&toml).expect_err("zero test override must fail");
        assert!(format!("{err}").contains("observability.disk_usage_tick_seconds_test_override"));
    }

    #[test]
    fn zero_disk_capacity_bytes_is_rejected() {
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        let toml = format!(
            r#"
[server]
bind = "127.0.0.1:8088"

[auth]
token = "{token}"
session_salt = "{salt}"

[storage]
data_dir = "/var/lib/php-tree-viz"
retention_days = 30
disk_capacity_bytes = 0
"#
        );
        let err = parse(&toml).expect_err("zero disk_capacity_bytes must fail");
        let msg = format!("{err}");
        assert!(msg.contains("storage.disk_capacity_bytes"), "{msg}");
    }

    #[test]
    fn example_file_omits_test_only_override_key() {
        // The test-only override is documented intentionally absent
        // from the operator example so an operator never sets it by
        // accident. Mirror the retention.tick_seconds rule.
        assert!(
            !EXAMPLE_FILE.contains("disk_usage_tick_seconds_test_override"),
            "example file leaked the test-only override key"
        );
    }

    #[test]
    fn unknown_log_format_is_rejected() {
        let toml = replace_field(&full_toml(), "format", "\"yaml\"");
        let err = parse(&toml).expect_err("bad log.format must fail");
        let msg = format!("{err}");
        assert!(msg.contains("log.format"));
        assert!(msg.contains("json"));
        assert!(msg.contains("text"));
    }

    // ---- Debug never leaks secrets ----

    #[test]
    fn config_debug_does_not_contain_token_or_salt() {
        let cfg = parse(&full_toml()).unwrap();
        let rendered = format!("{cfg:?}");
        let token = "a".repeat(40);
        let salt = "b".repeat(40);
        assert!(
            !rendered.contains(&token),
            "config Debug leaked the token: {rendered}"
        );
        assert!(
            !rendered.contains(&salt),
            "config Debug leaked the salt: {rendered}"
        );
        // Sanity: the redaction marker should be present (twice).
        assert!(rendered.matches("***").count() >= 2, "{rendered}");
    }

    // ---- I/O via load_from_path ----

    #[test]
    fn load_from_path_returns_io_error_for_nonexistent() {
        let path = std::path::PathBuf::from("/definitely/does/not/exist.toml");
        let err = load_from_path(&path).expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains(path.to_str().unwrap()));
    }

    #[test]
    fn load_from_path_round_trips_via_a_temp_file() {
        let dir = make_unique_tempdir("load_from_path_round_trip");
        let path = dir.join("collector.toml");
        std::fs::write(&path, full_toml()).unwrap();
        let cfg = load_from_path(&path).expect("must load");
        assert_eq!(cfg.server.bind, "127.0.0.1:8088");
    }

    pub(crate) fn make_unique_tempdir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "phptv-collector-{}-{}-{}",
            std::process::id(),
            label,
            n,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- Example file ----

    /// `etc/collector.toml.example` is included at compile time so the
    /// test does not depend on a relative filesystem path being correct
    /// when `cargo test` runs.
    const EXAMPLE_FILE: &str = include_str!("../../../../etc/collector.toml.example");

    #[test]
    fn example_file_mentions_every_required_section() {
        for section in [
            "[server]",
            "[auth]",
            "[storage]",
            "[finalize]",
            "[retention]",
            "[log]",
            "[observability]",
        ] {
            assert!(
                EXAMPLE_FILE.contains(section),
                "example file is missing {section}"
            );
        }
    }

    #[test]
    fn example_file_loads_after_replacing_placeholders() {
        let token = "T".repeat(40);
        let salt = "S".repeat(40);
        let body = EXAMPLE_FILE
            .replace("REPLACE_ME_TOO", &salt)
            .replace("REPLACE_ME", &token);
        let dir = make_unique_tempdir("example_loads");
        let path = dir.join("collector.toml");
        std::fs::write(&path, &body).unwrap();
        let cfg = load_from_path(&path).expect("example should load after substitution");
        assert_eq!(cfg.auth.token.expose_secret(), token);
        assert_eq!(cfg.auth.session_salt.expose_secret(), salt);
    }
}
