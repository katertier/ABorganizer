//! In-memory rolling-window rate limiter.
//!
//! Used today only on `POST /api/v1/pairing/consume` — the
//! single anonymous mutating endpoint in the API surface. The
//! consume path is gated by argon2id (~50ms / verify) so even
//! a CPU-bound attacker can only push ~12k attempts through a
//! code's 10-min lifetime; this limiter is a second-line guard
//! that caps the budget further before argon2id even runs, so
//! a flood doesn't soak daemon CPU.
//!
//! # Defence-in-depth posture
//!
//! Three layers protect the pairing flow:
//!
//! 1. **Code entropy** — `XXXX-XXXX` from a 22-letter alphabet
//!    = ~36 bits. Plenty for the threat model.
//! 2. **argon2id verify** — ~50ms per attempt forces the
//!    attacker to spend real CPU per guess. ~12k attempts max
//!    per 10-min code lifetime.
//! 3. **This rate limiter** — caps the rate at which attempts
//!    can be lodged at all, so a flood can't saturate the
//!    daemon's CPU on argon2id verifies. With the default
//!    `30 failures / 60s` budget, a malicious actor's
//!    effective attempt rate falls to ~one per two seconds
//!    after the burst — well below what's needed to make any
//!    dent in the 36-bit code space.
//!
//! # Why global, not per-IP
//!
//! `axum::extract::ConnectInfo<SocketAddr>` is available, but
//! the daemon is single-user (per project memory
//! `repo-public-because-ci-budget`) and the source IP plumbing
//! adds ceremony that doesn't change the threat model: a
//! determined attacker on the LAN can spoof IPs, so per-IP
//! limiting offers no real cap on aggregate attempts. A global
//! ceiling matches the threat (CPU-saturation flood) with the
//! least ceremony.
//!
//! The cost — a single legitimate device retrying many times
//! within 60s could lock other devices out — is acceptable for
//! the single-user / few-devices reality. If the design ever
//! grows to multi-user the per-tenant scoping problem becomes
//! interesting and a different limiter shape (per-user code-
//! issuer plus per-IP attempter) is the right answer.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default ceiling: max FAILED attempts per
/// [`RateLimiter::WINDOW`] before the next attempt gets a 429.
/// Successful pairings don't count.
pub const DEFAULT_PAIRING_CONSUME_LIMIT: usize = 30;

/// Rolling window for the failure counter. Anything older than
/// this gets pruned on every `check` / `record_failure` call.
pub const DEFAULT_PAIRING_CONSUME_WINDOW: Duration = Duration::from_secs(60);

/// Outcome of a [`RateLimiter::check`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// Attempt allowed — proceed with the protected operation.
    Allowed,
    /// Attempt rejected — return 429 with this `Retry-After`
    /// in seconds. Always ≥ 1.
    RateLimited { retry_after_secs: u64 },
}

/// In-memory rolling-window failure counter.
///
/// `Mutex<VecDeque<Instant>>` of recent failure timestamps:
/// `check` prunes anything older than `WINDOW`, returns
/// `RateLimited` when the surviving count >= `limit`. Allocation-
/// free in the common path (push to the back, pop from the
/// front).
///
/// Default is a 30-failures-per-60-seconds budget. Override per
/// site if the threat model warrants different numbers.
#[derive(Debug)]
pub struct RateLimiter {
    /// Recent failure timestamps in submission order.
    failures: Mutex<VecDeque<Instant>>,
    /// Max failures permitted within `window` before
    /// rate-limiting kicks in.
    limit: usize,
    /// Rolling-window length.
    window: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(
            DEFAULT_PAIRING_CONSUME_LIMIT,
            DEFAULT_PAIRING_CONSUME_WINDOW,
        )
    }
}

impl RateLimiter {
    /// Construct a limiter with explicit `limit` + `window`.
    /// Use [`Default::default`] for the production defaults.
    #[must_use]
    pub const fn new(limit: usize, window: Duration) -> Self {
        Self {
            failures: Mutex::new(VecDeque::new()),
            limit,
            window,
        }
    }

    /// Check the current bucket. Doesn't record anything — call
    /// [`Self::record_failure`] separately after the protected
    /// operation actually fails.
    ///
    /// Splitting the two means a 429 response doesn't itself
    /// count toward the budget (so a misconfigured client
    /// hammering the endpoint can't keep itself locked out
    /// indefinitely after the window expires).
    ///
    /// On a poisoned mutex (another thread panicked while
    /// holding the lock) the limiter degrades to
    /// [`CheckResult::Allowed`] — fail-open is the right
    /// default for a defence-in-depth layer: the underlying
    /// argon2id verify still gates the consume path, and a
    /// permanent 429 from a stale poison would be a worse UX
    /// than a brief defence drop while operators investigate.
    pub fn check(&self) -> CheckResult {
        let now = Instant::now();
        let Ok(mut q) = self.failures.lock() else {
            tracing::warn!("rate_limit.mutex_poisoned check fail_open");
            return CheckResult::Allowed;
        };
        // `Instant::now()` is monotonic but its origin is
        // implementation-defined; on a freshly-booted CI runner
        // the value can be smaller than `self.window` (e.g. 50ms
        // since system start with a 60s window). In that case
        // `checked_sub` returns None — and "the cutoff is
        // negative" means logically "nothing has aged out yet,"
        // so skip the prune entirely.
        //
        // The earlier `.unwrap_or(now)` fallback was a bug: it
        // set the cutoff TO `now`, causing the loop to prune
        // every entry whose timestamp was < now (i.e., every
        // entry). That caused `partial_age_out_partially_unlocks`
        // to flake on macOS CI runners.
        if let Some(cutoff) = now.checked_sub(self.window) {
            while let Some(&front) = q.front() {
                if front < cutoff {
                    q.pop_front();
                } else {
                    break;
                }
            }
        }
        // Compute the answer in this scope before dropping the
        // lock so the response variant doesn't keep the mutex
        // held across the return.
        let result = if q.len() >= self.limit {
            // Retry-After = how long until the OLDEST surviving
            // failure ages out of the window. Guarantees the
            // client backs off long enough that at least one
            // slot opens up.
            //
            // `front()` is guaranteed `Some` here because
            // `len() >= self.limit >= 1` — but match it rather
            // than expect() to keep this clippy-clean under the
            // workspace's `expect_used = "warn"` lint.
            let until_free = q.front().copied().map_or(self.window, |oldest| {
                (oldest + self.window).saturating_duration_since(now)
            });
            // saturating max(1) — `Retry-After: 0` is technically
            // valid but reads as "retry immediately" which
            // defeats the back-off; floor at 1 second.
            let retry_after_secs = until_free.as_secs().max(1);
            CheckResult::RateLimited { retry_after_secs }
        } else {
            CheckResult::Allowed
        };
        drop(q);
        result
    }

    /// Record a failed attempt. Doesn't check — the handler is
    /// responsible for calling [`Self::check`] first.
    ///
    /// On a poisoned mutex this is a silent no-op (same
    /// fail-open posture as [`Self::check`]).
    pub fn record_failure(&self) {
        let Ok(mut q) = self.failures.lock() else {
            tracing::warn!("rate_limit.mutex_poisoned record_failure fail_open");
            return;
        };
        q.push_back(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        reason = "test setup idioms"
    )]

    use super::{CheckResult, RateLimiter};
    use std::time::Duration;

    #[test]
    fn default_allows_first_attempt() {
        let rl = RateLimiter::default();
        assert_eq!(rl.check(), CheckResult::Allowed);
    }

    #[test]
    fn rejects_after_limit_reached() {
        // Tiny limit so the test fires quickly.
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        rl.record_failure();
        rl.record_failure();
        rl.record_failure();
        match rl.check() {
            CheckResult::RateLimited { retry_after_secs } => {
                assert!(
                    retry_after_secs >= 1,
                    "Retry-After must be at least 1s, got {retry_after_secs}"
                );
                // 60-second window, just recorded → retry-after
                // should be close to 60s.
                assert!(
                    retry_after_secs <= 60,
                    "Retry-After must be ≤ window, got {retry_after_secs}"
                );
            }
            CheckResult::Allowed => panic!("expected RateLimited after hitting limit"),
        }
    }

    #[test]
    fn check_is_non_recording() {
        // Calling check() repeatedly when over the limit must
        // NOT push more entries into the bucket — otherwise a
        // hammering client could keep its own bucket full
        // indefinitely and never unlock.
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        rl.record_failure();
        rl.record_failure();
        let first = rl.check();
        let second = rl.check();
        assert_eq!(first, second, "check() must be idempotent");
        // The retry-after value should be roughly the same on
        // consecutive calls (within a tolerance for elapsed
        // time between them).
        match (first, second) {
            (
                CheckResult::RateLimited {
                    retry_after_secs: a,
                },
                CheckResult::RateLimited {
                    retry_after_secs: b,
                },
            ) => {
                assert!(b <= a, "retry-after should not increase between calls");
            }
            _ => panic!("expected RateLimited from both calls"),
        }
    }

    #[test]
    fn ages_out_after_window() {
        // Window is 100ms so the test actually completes — much
        // shorter than the production 60s default.
        let rl = RateLimiter::new(2, Duration::from_millis(100));
        rl.record_failure();
        rl.record_failure();
        assert!(matches!(rl.check(), CheckResult::RateLimited { .. }));

        // Sleep past the window. The bucket should empty.
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            rl.check(),
            CheckResult::Allowed,
            "all entries should age out after the window"
        );
    }

    #[test]
    fn partial_age_out_partially_unlocks() {
        // Window is 100ms. Record 3 failures with one delayed
        // → the oldest two should age out while the newest
        // stays, leaving room for one more attempt.
        let rl = RateLimiter::new(3, Duration::from_millis(100));
        rl.record_failure();
        rl.record_failure();
        std::thread::sleep(Duration::from_millis(60));
        rl.record_failure();
        // 3 in the bucket, all within window → RateLimited.
        assert!(matches!(rl.check(), CheckResult::RateLimited { .. }));
        // Sleep past the first two but not the third.
        std::thread::sleep(Duration::from_millis(60));
        // First two are now > 120ms old (aged out); the third
        // is ~60ms old (within window). Bucket count = 1, below
        // limit of 3.
        assert_eq!(rl.check(), CheckResult::Allowed);
    }

    /// Regression test for the `Instant::checked_sub` underflow
    /// bug that flaked CI on macOS runners with recent
    /// boot-time monotonic clocks.
    ///
    /// When `now < window` (the runner just booted, or `Instant`
    /// origin is recent for any reason), the earlier
    /// `.unwrap_or(now)` fallback set the prune cutoff TO `now`,
    /// erroneously pruning every entry. The fix: skip the prune
    /// when the cutoff can't be computed.
    ///
    /// We can't actually force `Instant` to be small (the type
    /// is opaque). But the BEHAVIOUR we care about — "after
    /// recording N failures and immediately checking, the
    /// bucket isn't empty" — IS testable, and is the proxy
    /// invariant the production code relies on.
    #[test]
    fn check_immediately_after_record_does_not_prune() {
        // Use a 1-hour window so absolutely nothing should age
        // out between `record_failure` and `check`. If the prune
        // logic ever again accidentally treats "can't compute
        // cutoff" as "cutoff = now", this test breaks because
        // the bucket would empty during the prune.
        let rl = RateLimiter::new(2, Duration::from_secs(3600));
        rl.record_failure();
        rl.record_failure();
        assert!(
            matches!(rl.check(), CheckResult::RateLimited { .. }),
            "fresh failures within a long window must NOT be pruned by check()"
        );
    }
}
