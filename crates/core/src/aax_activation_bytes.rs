//! AAX activation-bytes resolver (ADR-0053).
//!
//! Three-path lookup chain, first match wins:
//!
//! 1. **Env var `ABORG_AAX_ACTIVATION_BYTES`** — highest
//!    precedence, useful for one-off / scripted runs.
//! 2. **`Tunables.audio.aax_activation_bytes`** — TOML-config
//!    leg, for declarative deployments.
//! 3. **macOS Keychain** under service
//!    `io.github.katertier.aborganizer`, account
//!    `aax_activation_bytes` — default persistent store.
//!
//! The resolver never logs the value. The [`Source`] enum is
//! the only piece returned for `aborg doctor` to surface; the
//! [`ActivationBytes`] newtype redacts in its `Debug`.

use serde::{Deserialize, Serialize};

use crate::tunables::AudioTunables;

/// Env-var lookup key (path 1).
pub const ENV_VAR: &str = "ABORG_AAX_ACTIVATION_BYTES";
/// Keychain `kSecAttrService` (path 3). Reads
/// [`crate::build_info::BUNDLE_ID_BASE`] so the service name
/// follows whatever domain + app-name the workspace metadata
/// declares.
#[must_use]
pub const fn keychain_service() -> &'static str {
    crate::build_info::BUNDLE_ID_BASE
}
/// Keychain `kSecAttrAccount` (path 3).
pub const KEYCHAIN_ACCOUNT: &str = "aax_activation_bytes";

/// Which lookup leg produced the resolved bytes. Surfaced by
/// `aborg doctor`'s status line so the operator can tell which
/// path won — never the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `ABORG_AAX_ACTIVATION_BYTES` env var.
    Env,
    /// `Tunables.audio.aax_activation_bytes` (config.toml).
    Config,
    /// macOS Keychain entry.
    Keychain,
}

impl Source {
    /// Short tag for `aborg doctor` output (`env` / `config` /
    /// `keychain`).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
            Self::Keychain => "keychain",
        }
    }
}

/// Validated 32-bit AAX activation key (8 lowercase hex chars).
///
/// Holds an owned `String` so accidentally cloning the value
/// stays attributable to the owner. `Debug` redacts; the only
/// way to see the bytes is `as_hex()` — call sites that hand
/// off the value to the FFI are the only legitimate consumers.
#[derive(Clone, PartialEq, Eq)]
pub struct ActivationBytes(String);

impl ActivationBytes {
    /// Parse + validate. Accepts upper- or lower-case hex;
    /// canonicalises to lowercase for stable Keychain round-trip.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] when the input isn't exactly 8 hex
    /// characters.
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty);
        }
        if trimmed.len() != 8 {
            return Err(ParseError::WrongLength {
                actual: trimmed.len(),
            });
        }
        if !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ParseError::NotHex);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// Lowercase hex string suitable for the AVFoundation FFI's
    /// `aax_decrypt_to_m4b(activation_bytes_hex)` parameter.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ActivationBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ActivationBytes(<redacted>)")
    }
}

/// `ActivationBytes::parse` rejections.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// Input was empty after trimming.
    #[error("activation bytes empty")]
    Empty,
    /// Input wasn't exactly 8 characters.
    #[error("activation bytes wrong length: {actual} (expected 8 hex chars)")]
    WrongLength {
        /// Characters seen (post-trim).
        actual: usize,
    },
    /// Input contained non-hex characters.
    #[error("activation bytes must be 8 hex chars (0-9 a-f A-F)")]
    NotHex,
}

/// Resolve activation bytes from the three-path chain. Returns
/// `None` when nothing is configured.
///
/// The lookup never logs the value; the [`Source`] return is
/// the only piece intended for user-visible output.
#[must_use]
pub fn resolve(audio: &AudioTunables) -> Option<(ActivationBytes, Source)> {
    let env_value = std::env::var(ENV_VAR).ok();
    resolve_with(env_value.as_deref(), audio, keychain::read)
}

/// Test-friendly resolver core. Same chain as [`resolve`] but
/// the env-var lookup is passed in (so tests don't have to
/// mutate process env) and the keychain reader is injectable.
///
/// Production [`resolve`] supplies `std::env::var(ENV_VAR)`
/// and [`keychain::read`]; tests pass closures returning known
/// values.
#[must_use]
pub fn resolve_with(
    env_value: Option<&str>,
    audio: &AudioTunables,
    keychain_reader: impl FnOnce() -> Option<String>,
) -> Option<(ActivationBytes, Source)> {
    if let Some(v) = env_value {
        if let Ok(b) = ActivationBytes::parse(v) {
            return Some((b, Source::Env));
        }
    }
    if let Some(v) = audio.aax_activation_bytes.as_deref() {
        if let Ok(b) = ActivationBytes::parse(v) {
            return Some((b, Source::Config));
        }
    }
    if let Some(v) = keychain_reader() {
        if let Ok(b) = ActivationBytes::parse(&v) {
            return Some((b, Source::Keychain));
        }
    }
    None
}

/// macOS Keychain backend (path 3). Wraps
/// `security_framework::passwords` with the project-specific
/// service / account constants and a typed error.
pub mod keychain {
    use super::{ActivationBytes, KEYCHAIN_ACCOUNT, keychain_service};

    /// Read the activation-bytes entry from the macOS Keychain.
    /// Returns the raw stored string (not yet validated) so the
    /// resolver can decide what to do with malformed entries.
    ///
    /// Returns `None` when the entry doesn't exist OR when any
    /// Keychain API call fails. Failures are never propagated to
    /// the caller because the missing-or-broken cases are
    /// indistinguishable from the operator's standpoint —
    /// `aborg doctor` reports "not configured" in both, which is
    /// the correct user-facing signal.
    #[must_use]
    pub fn read() -> Option<String> {
        security_framework::passwords::get_generic_password(keychain_service(), KEYCHAIN_ACCOUNT)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    }

    /// Keychain write / delete failure. The error carries the
    /// `OSStatus` from the `security_framework` crate so the CLI
    /// can decide whether to retry or surface a "permission
    /// denied / locked keychain" hint.
    ///
    /// The error variant carries no user data.
    #[derive(Debug, thiserror::Error)]
    pub enum KeychainError {
        /// Underlying [`security_framework`] error. The
        /// `Display` impl includes the `OSStatus` code but never
        /// any user data.
        #[error("keychain error: {0}")]
        Framework(#[from] security_framework::base::Error),
    }

    /// Store the activation bytes in the macOS Keychain. The
    /// stored form is the lowercase hex string from
    /// `bytes.as_hex()`; never the raw 4-byte value.
    ///
    /// Overwrites any existing entry for
    /// (`KEYCHAIN_SERVICE`, `KEYCHAIN_ACCOUNT`).
    ///
    /// # Errors
    ///
    /// Surfaces any Keychain API error from
    /// `security_framework::passwords::set_generic_password`.
    pub fn set(bytes: &ActivationBytes) -> Result<(), KeychainError> {
        security_framework::passwords::set_generic_password(
            keychain_service(),
            KEYCHAIN_ACCOUNT,
            bytes.as_hex().as_bytes(),
        )?;
        Ok(())
    }

    /// Remove the activation-bytes Keychain entry. Idempotent:
    /// no-op when the entry doesn't exist.
    ///
    /// # Errors
    ///
    /// Surfaces any Keychain API error other than "not found".
    pub fn forget() -> Result<(), KeychainError> {
        match security_framework::passwords::delete_generic_password(
            keychain_service(),
            KEYCHAIN_ACCOUNT,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                // The framework returns errSecItemNotFound when
                // the entry doesn't exist; treat as success.
                if e.code() == -25_300 {
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn audio_with(config_value: Option<&str>) -> AudioTunables {
        AudioTunables {
            aax_activation_bytes: config_value.map(str::to_owned),
        }
    }

    #[test]
    fn parse_accepts_lowercase_hex() {
        let b = ActivationBytes::parse("1e332406").expect("valid");
        assert_eq!(b.as_hex(), "1e332406");
    }

    #[test]
    fn parse_canonicalises_uppercase_to_lowercase() {
        let b = ActivationBytes::parse("1E332406").expect("valid");
        assert_eq!(b.as_hex(), "1e332406");
    }

    #[test]
    fn parse_trims_whitespace() {
        let b = ActivationBytes::parse("  1e332406  \n").expect("valid");
        assert_eq!(b.as_hex(), "1e332406");
    }

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(ActivationBytes::parse("").unwrap_err(), ParseError::Empty);
        assert_eq!(
            ActivationBytes::parse("   \n").unwrap_err(),
            ParseError::Empty
        );
    }

    #[test]
    fn parse_rejects_wrong_length() {
        let err = ActivationBytes::parse("1e33240").unwrap_err();
        assert_eq!(err, ParseError::WrongLength { actual: 7 });
        let err = ActivationBytes::parse("1e3324066").unwrap_err();
        assert_eq!(err, ParseError::WrongLength { actual: 9 });
    }

    #[test]
    fn parse_rejects_non_hex() {
        assert_eq!(
            ActivationBytes::parse("1e33240g").unwrap_err(),
            ParseError::NotHex
        );
        assert_eq!(
            ActivationBytes::parse("xxxxxxxx").unwrap_err(),
            ParseError::NotHex
        );
    }

    #[test]
    fn debug_redacts() {
        let b = ActivationBytes::parse("deadbeef").unwrap();
        let dbg = format!("{b:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("deadbeef"));
    }

    #[test]
    fn source_tag_strings() {
        assert_eq!(Source::Env.tag(), "env");
        assert_eq!(Source::Config.tag(), "config");
        assert_eq!(Source::Keychain.tag(), "keychain");
    }

    #[test]
    fn resolve_env_wins_over_config_and_keychain() {
        let audio = audio_with(Some("aaaaaaaa"));
        let got = resolve_with(Some("1e332406"), &audio, || Some("bbbbbbbb".to_owned()));
        let (bytes, src) = got.expect("resolved");
        assert_eq!(bytes.as_hex(), "1e332406");
        assert_eq!(src, Source::Env);
    }

    #[test]
    fn resolve_config_wins_over_keychain() {
        let audio = audio_with(Some("aaaaaaaa"));
        let got = resolve_with(None, &audio, || Some("bbbbbbbb".to_owned()));
        let (bytes, src) = got.expect("resolved");
        assert_eq!(bytes.as_hex(), "aaaaaaaa");
        assert_eq!(src, Source::Config);
    }

    #[test]
    fn resolve_falls_through_to_keychain() {
        let audio = audio_with(None);
        let got = resolve_with(None, &audio, || Some("bbbbbbbb".to_owned()));
        let (bytes, src) = got.expect("resolved");
        assert_eq!(bytes.as_hex(), "bbbbbbbb");
        assert_eq!(src, Source::Keychain);
    }

    #[test]
    fn resolve_returns_none_when_nothing_set() {
        let audio = audio_with(None);
        assert!(resolve_with(None, &audio, || None).is_none());
    }

    #[test]
    fn resolve_skips_malformed_env_falls_to_config() {
        let audio = audio_with(Some("1e332406"));
        let got = resolve_with(Some("not-hex"), &audio, || None);
        let (bytes, src) = got.expect("resolved");
        assert_eq!(bytes.as_hex(), "1e332406");
        assert_eq!(src, Source::Config);
    }

    #[test]
    fn resolve_skips_malformed_config_falls_to_keychain() {
        let audio = audio_with(Some("garbage"));
        let got = resolve_with(None, &audio, || Some("1e332406".to_owned()));
        let (bytes, src) = got.expect("resolved");
        assert_eq!(bytes.as_hex(), "1e332406");
        assert_eq!(src, Source::Keychain);
    }

    #[test]
    fn resolve_returns_none_when_all_three_malformed() {
        let audio = audio_with(Some("garbage"));
        assert!(resolve_with(Some("nope"), &audio, || Some("also-bad".to_owned())).is_none());
    }
}
