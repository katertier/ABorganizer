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
//! every authenticated request pay 50–200 ms of CPU. For random
//! API tokens, use [`hash_api_token`] / [`verify_api_token`]
//! below (blake3) — keyed cryptographic hash at storage time so
//! a leaked DB doesn't directly leak the token, and the
//! per-request cost is just a fast hash.
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

// ── Fast hash — API tokens ──────────────────────────────────────────
//
// blake3 wrappers for **high-entropy random** API tokens. Backlog
// item 4a uses these to store `tokens.token_hash` at rest and
// hash the presented bearer on every authenticated request.
//
// We use **plain blake3 of the bare token**, not keyed/MAC
// blake3, on purpose:
//
// 1. **Determinism**. Auth middleware looks up by `WHERE
//    token_hash = ?`. That demands the same input → same output;
//    keyed blake3 with a server-side key works too but couples
//    the hash space to a key the operator now has to back up,
//    rotate, and supply to every restart. The threat we're
//    defending against is "DB leak directly leaks the token";
//    plain blake3 of a 256-bit-entropy input is already
//    cryptographically infeasible to invert, so the extra key
//    buys nothing in the random-bearer scenario.
//
// 2. **Rotation friction**. A keyed scheme means rotating the
//    server key invalidates every stored token. With plain
//    blake3, key compromise isn't a concept and rotation
//    happens per-token (operator revokes + re-issues).
//
// **Caveat**: if we ever stored low-entropy tokens here
// (4-digit PIN, 8-char pairing code), plain blake3 would be
// brute-forceable from a leaked DB. That's exactly the case
// where the [`hash_password`] / [`verify_password`] pair above
// is the right choice. The fast-hash surface assumes
// random-bytes input.

/// Mint a fresh 32-byte (256-bit) random API token and return
/// it as 64 lower-case hex chars.
///
/// 256 bits of entropy is enough that brute-force is infeasible
/// regardless of hash speed — that's why
/// [`hash_api_token`] is allowed to be the fast hash. Callers
/// store [`hash_api_token`] of the return value; the raw token
/// is shown to the operator exactly once at issue time and
/// never persisted.
#[must_use]
pub fn mint_api_token() -> String {
    use argon2::password_hash::rand_core::RngCore;
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for b in bytes {
        // Manual hex encode keeps us free of the `hex` crate
        // dep for this two-line helper.
        const NIBBLES: &[u8; 16] = b"0123456789abcdef";
        out.push(NIBBLES[(b >> 4) as usize] as char);
        out.push(NIBBLES[(b & 0x0f) as usize] as char);
    }
    out
}

/// Hash a high-entropy API token (random bytes, e.g. 32 bytes
/// hex-encoded as a 64-char string) for storage at rest.
///
/// Returns the lower-case hex of the blake3 digest. Caller stores
/// the hex string in `tokens.token_hash` (column is `TEXT
/// UNIQUE`); raw token bytes never persist.
///
/// Same input always produces the same output — that's the
/// **point** for the auth lookup path. Do NOT use this for
/// low-entropy material; see module docs.
#[must_use]
pub fn hash_api_token(plaintext: &str) -> String {
    blake3::hash(plaintext.as_bytes()).to_hex().to_string()
}

/// Constant-time check that `plaintext`'s blake3 hash equals
/// `expected_hex` (a previous [`hash_api_token`] output).
///
/// The byte-compare is constant-time within blake3's `Hash`
/// type's `PartialEq`; the hex decode of `expected_hex`
/// length-mismatches in non-constant time (decode of 64-char
/// hex into 32-byte buffer), but the only thing that leak
/// reveals is "the stored hex is malformed," which is a server
/// config bug, not a user-controllable input.
///
/// Returns `false` for a malformed `expected_hex` (wrong length
/// or non-hex chars). That's not an error path — the caller's
/// "token rejected" branch handles it the same way as a
/// mismatch.
#[must_use]
pub fn verify_api_token(plaintext: &str, expected_hex: &str) -> bool {
    let computed = blake3::hash(plaintext.as_bytes());
    // Decode `expected_hex` into a 32-byte buffer; reject any
    // length / encoding mismatch up front.
    let Ok(decoded_bytes) = decode_blake3_hex(expected_hex) else {
        return false;
    };
    let expected = blake3::Hash::from_bytes(decoded_bytes);
    // `Hash`'s PartialEq is constant-time via subtle::CtOption.
    computed == expected
}

/// Decode a 64-char hex string into the 32-byte blake3 digest
/// buffer. Returns `Err(())` on any malformation — the auth
/// path treats malformed stored hashes as a non-match.
fn decode_blake3_hex(hex: &str) -> Result<[u8; 32], ()> {
    if hex.len() != 64 {
        return Err(());
    }
    let mut out = [0_u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        if chunk.len() != 2 {
            return Err(());
        }
        let hi = nibble(chunk[0])?;
        let lo = nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

const fn nibble(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
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

    // ── Fast-hash (blake3) — API tokens ───────────────────────────

    #[test]
    fn hash_api_token_is_deterministic() {
        // Same input → same output. This is required for the
        // auth middleware's lookup-by-hash path.
        let token = "aabbccddeeff00112233445566778899aabbccddeeff0011";
        let a = hash_api_token(token);
        let b = hash_api_token(token);
        assert_eq!(a, b, "blake3 is deterministic");
        // Output is 64 lower-case hex chars (32-byte digest).
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn hash_api_token_differs_per_token() {
        let a = hash_api_token("token-a");
        let b = hash_api_token("token-b");
        assert_ne!(a, b);
    }

    #[test]
    fn verify_api_token_round_trips() {
        let token = "deadbeefcafebabe1234567890abcdef0123456789abcdef0123456789abcdef";
        let stored = hash_api_token(token);
        assert!(verify_api_token(token, &stored));
    }

    #[test]
    fn verify_api_token_rejects_mismatch() {
        let stored = hash_api_token("correct");
        assert!(!verify_api_token("wrong", &stored));
    }

    #[test]
    fn verify_api_token_rejects_malformed_stored() {
        // Wrong length.
        assert!(!verify_api_token("anything", "abc"));
        // Right length, non-hex chars.
        let bad = "z".repeat(64);
        assert!(!verify_api_token("anything", &bad));
        // Right length, almost-hex but with caps + a typo.
        let almost = "A".repeat(63) + "Z";
        assert!(!verify_api_token("anything", &almost));
    }

    #[test]
    fn mint_api_token_shape() {
        let t = mint_api_token();
        assert_eq!(t.len(), 64);
        assert!(
            t.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn mint_api_token_is_distinct_per_call() {
        // Same RNG-based smoke test as the salt-collision guard.
        let a = mint_api_token();
        let b = mint_api_token();
        assert_ne!(a, b, "RNG collision — almost certainly a bug");
    }

    #[test]
    fn mint_then_hash_then_verify_round_trips() {
        let token = mint_api_token();
        let stored = hash_api_token(&token);
        assert!(verify_api_token(&token, &stored));
        assert!(!verify_api_token(&mint_api_token(), &stored));
    }

    #[test]
    fn verify_api_token_accepts_uppercase_hex_stored() {
        // We store lower-case but accept either on the verify
        // path — a tools-side dump might re-case the column.
        let token = "round-trip-token";
        let lower = hash_api_token(token);
        let upper = lower.to_ascii_uppercase();
        assert!(
            verify_api_token(token, &upper),
            "upper-case hex still matches"
        );
    }
}
