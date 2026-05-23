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
/// `SPECIFICATION.md` §7.3.
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finalize {
    #[serde(default = "defaults::idle_seconds")]
    pub idle_seconds: u32,
    #[serde(default = "defaults::tick_seconds")]
    pub tick_seconds: u32,
}

impl Default for Finalize {
    fn default() -> Self {
        Self {
            idle_seconds: defaults::idle_seconds(),
            tick_seconds: defaults::tick_seconds(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    #[serde(default = "defaults::tick_minutes")]
    pub tick_minutes: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            tick_minutes: defaults::tick_minutes(),
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
    pub(super) fn tick_minutes() -> u32 {
        60 // §7.3
    }
    pub(super) fn log_level() -> String {
        "info".to_owned() // §7.3
    }
    pub(super) fn log_format() -> String {
        "json".to_owned() // §7.3: "or 'text'"
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
    fn zero_tick_minutes_is_rejected() {
        let toml = replace_field(&full_toml(), "tick_minutes", "0");
        let err = parse(&toml).expect_err("zero tick_minutes must fail");
        assert!(format!("{err}").contains("retention.tick_minutes"));
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
