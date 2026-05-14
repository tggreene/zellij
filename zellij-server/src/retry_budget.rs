//! Bounded retry budget for the per-client route thread.
//!
//! When the route thread receives instructions before the session is
//! fully initialised (`session_data` is `None`), the affected
//! `ClientToServerMsg` is pushed into a retry queue and replayed on
//! the next loop iteration. Historically that drain loop slept a
//! fixed 5ms between attempts and had no upper bound: if a session
//! got stuck with `session_data == None` permanently (e.g. the Screen
//! thread wedged), the route thread would burn ~200 retries/sec
//! forever, spamming `Server not ready, trying to place instruction
//! in retry queue...` / `Server ready, retrying sending instruction.`
//! into the log and pegging a core. `zellij list-sessions` and any
//! interactive client would hang because no progress was ever made.
//!
//! `RetryBudget` is the circuit breaker that prevents that livelock.
//! The route thread calls [`RetryBudget::record_attempt`] once per
//! retry; once the budget is exhausted the caller is expected to bail
//! out and disconnect the client (mirroring the pre-existing
//! `consecutive_unknown_messages_received >= 1000` guard further down
//! the same loop). Backoff grows linearly from the base interval up
//! to the configured cap so a short-lived startup race still
//! completes quickly while a wedged session is killed in bounded
//! time.

use std::time::Duration;

/// Conservative defaults for the route-thread retry loop.
///
/// At the default cadence (5ms → 100ms), exhausting the budget takes
/// roughly 25–30 seconds — more than enough headroom for a slow
/// session startup, but a hard wall against the previous unbounded
/// livelock.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 400;
pub const DEFAULT_LOG_EVERY: u32 = 50;
pub const DEFAULT_BASE_BACKOFF_MS: u64 = 5;
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 100;

#[derive(Debug, Clone)]
pub struct RetryBudget {
    consecutive_attempts: u32,
    max_attempts: u32,
    log_every: u32,
    base_backoff_ms: u64,
    max_backoff_ms: u64,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_ATTEMPTS,
            DEFAULT_LOG_EVERY,
            DEFAULT_BASE_BACKOFF_MS,
            DEFAULT_MAX_BACKOFF_MS,
        )
    }
}

impl RetryBudget {
    pub fn new(
        max_attempts: u32,
        log_every: u32,
        base_backoff_ms: u64,
        max_backoff_ms: u64,
    ) -> Self {
        Self {
            consecutive_attempts: 0,
            max_attempts,
            log_every,
            base_backoff_ms,
            max_backoff_ms: max_backoff_ms.max(base_backoff_ms),
        }
    }

    /// Record one retry attempt. Returns `true` while there is still
    /// budget left, `false` once the cap is exceeded and the caller
    /// must bail out.
    pub fn record_attempt(&mut self) -> bool {
        self.consecutive_attempts = self.consecutive_attempts.saturating_add(1);
        self.consecutive_attempts <= self.max_attempts
    }

    /// Reset the budget. Called when an instruction is successfully
    /// delivered (i.e. not re-queued by the receiving thread).
    pub fn reset(&mut self) {
        self.consecutive_attempts = 0;
    }

    /// True when the current attempt deserves a log line. Always
    /// fires on the very first attempt of a burst and then every
    /// `log_every` attempts afterwards, keeping the route log
    /// readable instead of a wall of identical warnings.
    pub fn should_log(&self) -> bool {
        if self.log_every == 0 {
            return false;
        }
        self.consecutive_attempts == 1 || self.consecutive_attempts % self.log_every == 0
    }

    /// Linearly-scaled, capped backoff between retries.
    pub fn backoff(&self) -> Duration {
        let mult = u64::from(self.consecutive_attempts.max(1));
        let ms = self.base_backoff_ms.saturating_mul(mult).min(self.max_backoff_ms);
        Duration::from_millis(ms)
    }

    pub fn attempts(&self) -> u32 {
        self.consecutive_attempts
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_attempt_returns_true_until_budget_exhausted() {
        let mut budget = RetryBudget::new(3, 1, 1, 10);
        assert!(budget.record_attempt(), "attempt 1 should fit");
        assert!(budget.record_attempt(), "attempt 2 should fit");
        assert!(budget.record_attempt(), "attempt 3 should fit");
        assert!(!budget.record_attempt(), "attempt 4 exceeds the budget");
        assert!(!budget.record_attempt(), "stays exhausted");
    }

    #[test]
    fn reset_restores_full_budget() {
        let mut budget = RetryBudget::new(2, 1, 1, 10);
        assert!(budget.record_attempt());
        assert!(budget.record_attempt());
        assert!(!budget.record_attempt());
        budget.reset();
        assert_eq!(budget.attempts(), 0);
        assert!(budget.record_attempt(), "post-reset budget is fresh");
    }

    #[test]
    fn should_log_fires_on_first_attempt_and_every_log_every_afterwards() {
        let mut budget = RetryBudget::new(100, 10, 1, 10);
        let mut logged = Vec::new();
        for _ in 0..25 {
            budget.record_attempt();
            if budget.should_log() {
                logged.push(budget.attempts());
            }
        }
        assert_eq!(logged, vec![1, 10, 20], "log on first attempt then every 10th");
    }

    #[test]
    fn should_log_disabled_when_log_every_is_zero() {
        let mut budget = RetryBudget::new(10, 0, 1, 10);
        for _ in 0..5 {
            budget.record_attempt();
            assert!(!budget.should_log(), "log_every=0 silences all logging");
        }
    }

    #[test]
    fn backoff_scales_linearly_and_caps() {
        let budget_factory = || RetryBudget::new(100, 1, 5, 50);

        let mut b = budget_factory();
        // pre-attempt backoff still returns the base interval so the
        // first sleep is sensible
        assert_eq!(b.backoff(), Duration::from_millis(5));

        b.record_attempt();
        assert_eq!(b.backoff(), Duration::from_millis(5), "attempt 1 = base");

        b.record_attempt();
        assert_eq!(b.backoff(), Duration::from_millis(10), "attempt 2 = 2× base");

        for _ in 0..20 {
            b.record_attempt();
        }
        assert_eq!(b.backoff(), Duration::from_millis(50), "attempts >> cap stay at cap");
    }

    #[test]
    fn max_backoff_cannot_undercut_base() {
        // Guard against a misconfiguration where the cap is lower
        // than the base — the cap should be raised to the base,
        // never the other way around.
        let b = RetryBudget::new(10, 1, 20, 5);
        assert_eq!(b.backoff(), Duration::from_millis(20));
    }

    #[test]
    fn defaults_bound_total_wait_in_seconds_not_forever() {
        // Sanity check: with defaults the total worst-case wait
        // before bailing is on the order of tens of seconds, not
        // hours. (Sum of capped backoffs, conservative bound.)
        let budget = RetryBudget::default();
        let worst_case_ms =
            u64::from(budget.max_attempts()) * DEFAULT_MAX_BACKOFF_MS;
        assert!(
            worst_case_ms < 60_000,
            "worst case {worst_case_ms}ms must stay under a minute"
        );
        assert!(
            worst_case_ms > 1_000,
            "worst case {worst_case_ms}ms must give startup races real headroom"
        );
    }

    #[test]
    fn record_attempt_does_not_panic_at_u32_max() {
        // saturating_add prevents wraparound at attempt = u32::MAX
        // even if a caller forgets to bail when record_attempt returns false.
        let mut b = RetryBudget::new(10, 0, 5, 50);
        b.consecutive_attempts = u32::MAX - 1;
        assert!(!b.record_attempt(), "already over budget");
        assert!(!b.record_attempt(), "saturated, still over budget, no panic");
        assert_eq!(b.attempts(), u32::MAX);
    }

    #[test]
    fn backoff_multiplication_does_not_panic_with_large_base() {
        // saturating_mul guards against u64 overflow when a bad
        // config combines a huge base with a high attempt count.
        let mut b = RetryBudget::new(u32::MAX, 0, u64::MAX / 2, u64::MAX);
        b.consecutive_attempts = u32::MAX;
        // The point is: this call must not panic. Resulting Duration
        // is some saturated value, but the route loop will have
        // bailed via record_attempt long before reaching attempts
        // this high.
        let _ = b.backoff();
    }
}
