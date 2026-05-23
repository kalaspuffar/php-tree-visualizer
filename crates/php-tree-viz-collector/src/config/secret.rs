//! A string that refuses to print itself.
//!
//! Wraps `auth.token` and `auth.session_salt` from the collector config so
//! that neither `Debug` nor `Display` ever exposes the underlying value.
//! `Serialize` is deliberately not implemented — round-tripping a secret
//! to TOML or JSON is never the right answer for this type.
//!
//! Implements `SPECIFICATION.md` INV-2 ("the collector never reads or
//! logs the Authorization header content"), extended to cover the on-disk
//! secrets that gate it.

use std::fmt;

use serde::{Deserialize, Deserializer};

/// A string carrying a secret value. `Debug` and `Display` print only
/// `***`; the underlying bytes are reachable only through
/// [`SecretString::expose_secret`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Returns the underlying secret. The single intentional escape
    /// hatch — every caller is expected to use the value immediately
    /// (e.g. to compare against an `Authorization` header) rather than
    /// log it or clone it into a non-secret container.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Character count of the secret. Used by validation to enforce the
    /// "≥ 32 characters" rule without exposing the value.
    pub fn char_count(&self) -> usize {
        self.0.chars().count()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "hunter2hunter2hunter2hunter2hunter2hunter2";

    #[test]
    fn debug_does_not_expose_the_inner_value() {
        let secret = SecretString::from(SAMPLE);
        let rendered = format!("{secret:?}");
        assert!(
            !rendered.contains("hunter2"),
            "Debug output leaked the secret: {rendered}"
        );
        assert!(rendered.contains("***"));
    }

    #[test]
    fn display_does_not_expose_the_inner_value() {
        let secret = SecretString::from(SAMPLE);
        let rendered = format!("{secret}");
        assert!(
            !rendered.contains("hunter2"),
            "Display output leaked the secret: {rendered}"
        );
        assert_eq!(rendered, "***");
    }

    #[test]
    fn expose_secret_returns_the_underlying_string() {
        let secret = SecretString::from(SAMPLE);
        assert_eq!(secret.expose_secret(), SAMPLE);
    }

    #[test]
    fn char_count_matches_chars_of_inner() {
        assert_eq!(
            SecretString::from(SAMPLE).char_count(),
            SAMPLE.chars().count()
        );
        assert_eq!(SecretString::from("åäö").char_count(), 3);
    }

    #[test]
    fn deserialize_accepts_any_string() {
        let secret: SecretString = toml::from_str::<toml::Value>(&format!("v = \"{SAMPLE}\""))
            .unwrap()
            .as_table()
            .unwrap()
            .get("v")
            .unwrap()
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(secret.expose_secret(), SAMPLE);
    }

    #[test]
    fn equality_compares_underlying_string() {
        assert_eq!(SecretString::from("a"), SecretString::from("a"));
        assert_ne!(SecretString::from("a"), SecretString::from("b"));
    }
}
