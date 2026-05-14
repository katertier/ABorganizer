//! Password-hash primitives — argon2id wrappers.
//!
//! Backlog item 2 of the security cluster. Adds the **primitive
//! only** — no wiring into the admin-token middleware (that gets
//! replaced wholesale in backlog item 4: per-user tokens +
//! pairing flow). Item 4 will reach for [`hash_password`] /
//! [`verify_password`] here.
//!
//! # When to use argon2id
//!
//! argon2id is the OWASP-recommended PHF for **low-entropy
//! secret material** — user passwords, short pairing codes
//! (~6-8 chars), anything where the attacker's offline brute-
//! force budget matters. The slow-by-design property is the
//! whole point: each verify costs ~50–200 ms (depends on
//! params), so a stolen hash DB takes years to brute-force at
//! GPU rates.
//!
//! # When NOT to use argon2id
//!
//! **High-entropy random API tokens** (e.g. a 256-bit random
//! bearer token). Slow hashing buys nothing — brute-forcing 256
//! bits is infeasible regardless of hash speed — and it makes
//! every authenticated request pay 50–200 ms of CPU. For
//! random API tokens, hash with a **fast** function (sha256,
//! blake3) at storage time so a leaked DB doesn't directly leak
//! the token, and accept the request-rate cost is just a fast
//! hash. ABorganizer will add a `hash_api_token` /
//! `verify_api_token` companion when the per-user-tokens slice
//! lands; this module covers the slow-hash side only.
//!
//! # Output format
//!
//! [`hash_password`] returns the **PHC string format** — a
//! single self-describing line of the form:
//!
//! ```text
//! $argon2id$v=19$m=19456,t=2,p=1$<salt-b64>$<hash-b64>
//! ```
//!
//! Params (m, t, p) + salt are embedded, so verifying doesn't
//! need them as separate inputs. This is the format the
//! `password-hash` crate (re-exported by `argon2`) standardises.
//! Persist the encoded string as-is.
//!
//! # Default params
//!
//! [`Argon2::default()`] in argon2 0.5 maps to OWASP's "minimum
//! recommended" tier:
//!
//! | Param | Value | Note                                          |
//! |-------|-------|-----------------------------------------------|
//! | `m`   | 19456 | `KiB` of memory (~19 `MiB`) — fits on every host |
//! | `t`   | 2     | Time cost (iteration count)                   |
//! | `p`   | 1     | Lanes (single-thread)                         |
//!
//! These give ~50 ms per verify on Apple Silicon — slow enough
//! to defeat brute force, fast enough that a human pairing-code
//! verification doesn't feel laggy. We don't expose the params
//! as a tunable in this slice; if a future operator needs
//! tighter / looser settings, add an `Argon2Tunables` struct
//! then.

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

/// Typed errors from [`hash_password`] / [`verify_password`].
///
/// We don't re-export `argon2::password_hash::Error` directly so
/// the public surface stays stable across argon2 minor bumps;
/// callers match on these variants instead.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    /// argon2 couldn't compute the hash (rare — happens on
    /// memory-allocation failure or invalid params).
    #[error("argon2 hash failed: {0}")]
    HashFailed(String),
    /// The PHC string in [`verify_password`] couldn't be parsed
    /// — wrong format, truncated, or a non-argon2 algorithm.
    #[error("invalid PHC encoded password hash: {0}")]
    InvalidEncoded(String),
}

/// Hash a low-entropy secret (password, pairing code) with
/// argon2id and OWASP-recommended default params.
///
/// Returns a PHC-format encoded string (see module docs for
/// shape). Salt is generated per-call from `OsRng` — never reuse
/// salts.
///
/// # Errors
///
/// [`PasswordError::HashFailed`] when argon2's internal
/// hashing fails (rare; out-of-memory or invalid params).
///
/// # Example
///
/// ```
/// use ab_core::auth::{hash_password, verify_password};
///
/// let encoded = hash_password("hunter2").expect("hash");
/// assert!(verify_password("hunter2", &encoded).expect("verify"));
/// assert!(!verify_password("wrong", &encoded).expect("verify"));
/// ```
pub fn hash_password(plaintext: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::HashFailed(e.to_string()))
}

/// Verify a plaintext secret against a previously-stored PHC
/// encoded hash.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch,
/// [`PasswordError::InvalidEncoded`] if `encoded` isn't a valid
/// PHC string this argon2 build understands. **Verification is
/// constant-time** within argon2's password-hash crate —
/// returning `Ok(false)` doesn't leak how much of the hash
/// matched.
///
/// # Errors
///
/// [`PasswordError::InvalidEncoded`] when `encoded` fails to
/// parse as a PHC string (typo, truncation, algorithm not
/// argon2id). A mismatched-but-valid hash returns `Ok(false)`
/// — that's not an error.
///
/// # Example
///
/// See [`hash_password`].
pub fn verify_password(plaintext: &str, encoded: &str) -> Result<bool, PasswordError> {
    let parsed =
        PasswordHash::new(encoded).map_err(|e| PasswordError::InvalidEncoded(e.to_string()))?;
    match Argon2::default().verify_password(plaintext.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(PasswordError::InvalidEncoded(e.to_string())),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_succeeds_on_exact_match() {
        let enc = hash_password("correct horse battery staple").expect("hash");
        let ok = verify_password("correct horse battery staple", &enc).expect("verify");
        assert!(ok, "matching plaintext must verify true");
    }

    #[test]
    fn round_trip_fails_on_mismatch() {
        let enc = hash_password("hunter2").expect("hash");
        let ok = verify_password("hunter3", &enc).expect("verify");
        assert!(!ok, "wrong plaintext must verify false");
    }

    #[test]
    fn round_trip_handles_unicode_and_empty() {
        // Pairing codes might be ASCII-only by convention, but
        // user passwords could be anything. Round-trip both.
        for pw in ["", "ünıçöde 🔑", "  spaces  "] {
            let enc = hash_password(pw).expect("hash");
            assert!(
                verify_password(pw, &enc).expect("verify"),
                "round-trip {pw:?}"
            );
        }
    }

    #[test]
    fn hash_output_is_phc_format() {
        let enc = hash_password("anything").expect("hash");
        // PHC strings always start with `$argon2id$` for this
        // algorithm and embed `v=19$m=...$<salt>$<hash>`.
        assert!(enc.starts_with("$argon2id$"), "got {enc}");
        // Five $-separated segments after the leading $: algo,
        // version, params, salt, hash.
        let parts: Vec<_> = enc.split('$').collect();
        assert_eq!(
            parts.len(),
            6,
            "PHC format has 5 $-delimited fields after the leading $; got {parts:?}"
        );
        assert_eq!(parts[1], "argon2id");
        assert!(parts[2].starts_with("v="));
    }

    #[test]
    fn salt_differs_per_call_for_same_plaintext() {
        // OsRng-driven salt → distinct encodings for the same
        // input. Regression guard: a buggy salt source could
        // produce identical PHC strings and silently break the
        // "stolen one hash = cracks all" defense.
        let a = hash_password("same").expect("a");
        let b = hash_password("same").expect("b");
        assert_ne!(a, b, "salt collision — RNG is broken");
        // Both still verify the original plaintext.
        assert!(verify_password("same", &a).expect("verify a"));
        assert!(verify_password("same", &b).expect("verify b"));
    }

    #[test]
    fn verify_rejects_malformed_encoded() {
        let r = verify_password("anything", "not-a-phc-string");
        assert!(
            matches!(r, Err(PasswordError::InvalidEncoded(_))),
            "got {r:?}"
        );
    }

    #[test]
    fn verify_rejects_non_argon2_algo() {
        // bcrypt-style PHC string: parses as a PHC structure but
        // the argon2 verifier rejects the algorithm. Surfaces as
        // InvalidEncoded — caller treats this as a config bug,
        // not a wrong password.
        let bcrypt_phc = "$2b$10$abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyz0123";
        let r = verify_password("x", bcrypt_phc);
        assert!(
            matches!(r, Err(PasswordError::InvalidEncoded(_))),
            "got {r:?}"
        );
    }
}
