use serde::{Deserialize, Serialize};

/// A clock the limiter reads instead of the wall, so a soak test drives a
/// hundred simulated calls through a fake minute without sleeping.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        tenon_storage::now()
    }
}

const DAY_MS: i64 = 86_400_000;
const MINUTE_MS: i64 = 60_000;

/// Account rate control (RFC P5.0b, section 4): a token bucket well under the
/// plan's real limit, one serialized concurrency, a minimum gap plus jitter
/// between calls, and a daily cap. Defaults are deliberately conservative — the
/// cli-agent is a slow overnight explorer, not a fleet.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateConfig {
    #[serde(default = "rpm")]
    pub rpm: u32,
    #[serde(default = "rpd")]
    pub rpd: u32,
    #[serde(default = "min_gap_ms")]
    pub min_gap_ms: i64,
    #[serde(default = "jitter_ms")]
    pub jitter_ms: i64,
    #[serde(default = "concurrency")]
    pub concurrency: u32,
    #[serde(default = "breaker_threshold")]
    pub breaker_threshold: u32,
    #[serde(default = "breaker_max_opens")]
    pub breaker_max_opens: u32,
    #[serde(default = "backoff_base_ms")]
    pub backoff_base_ms: i64,
    #[serde(default = "backoff_max_ms")]
    pub backoff_max_ms: i64,
}

fn rpm() -> u32 {
    6
}
fn rpd() -> u32 {
    2000
}
fn min_gap_ms() -> i64 {
    3000
}
fn jitter_ms() -> i64 {
    2000
}
fn concurrency() -> u32 {
    1
}
fn breaker_threshold() -> u32 {
    3
}
fn breaker_max_opens() -> u32 {
    5
}
fn backoff_base_ms() -> i64 {
    2000
}
fn backoff_max_ms() -> i64 {
    300_000
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            rpm: rpm(),
            rpd: rpd(),
            min_gap_ms: min_gap_ms(),
            jitter_ms: jitter_ms(),
            concurrency: concurrency(),
            breaker_threshold: breaker_threshold(),
            breaker_max_opens: breaker_max_opens(),
            backoff_base_ms: backoff_base_ms(),
            backoff_max_ms: backoff_max_ms(),
        }
    }
}

/// The verdict on one `acquire`: run now, wait this many ms and ask again, or the
/// breaker has tripped `max_opens` times and the env must halt.
#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    Allow,
    Wait(i64),
    Halt(String),
}

/// What a reported failure did to the breaker: whether it opened this time and
/// whether the run has crossed `max_opens` and must halt with a violation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FailureOutcome {
    pub opened: bool,
    pub halted: bool,
}

pub struct Limiter {
    config: RateConfig,
    tokens: f64,
    last_refill: i64,
    last_grant: i64,
    minute_start: i64,
    minute_count: u32,
    day_start: i64,
    day_count: u32,
    in_flight: u32,
    grants: u64,
    consecutive_fails: u32,
    opens: u32,
    open_until: i64,
    halted: Option<String>,
}

impl Limiter {
    pub fn new(config: RateConfig) -> Self {
        let capacity = config.rpm.max(1) as f64;
        Self {
            config,
            tokens: capacity,
            last_refill: 0,
            last_grant: i64::MIN / 4,
            minute_start: 0,
            minute_count: 0,
            day_start: 0,
            day_count: 0,
            in_flight: 0,
            grants: 0,
            consecutive_fails: 0,
            opens: 0,
            open_until: 0,
            halted: None,
        }
    }

    /// The pacing gap for this grant: the fixed minimum plus a deterministic
    /// jitter in `[0, jitter_ms]`, so calls never land on an exact cadence a ToS
    /// heuristic could flag, yet a test still reproduces the sequence.
    fn gap(&self) -> i64 {
        if self.config.jitter_ms <= 0 {
            return self.config.min_gap_ms;
        }
        let mixed = self.grants.wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
        self.config.min_gap_ms + (mixed as i64).rem_euclid(self.config.jitter_ms + 1)
    }

    pub fn try_acquire(&mut self, clock: &dyn Clock) -> Grant {
        if let Some(reason) = &self.halted {
            return Grant::Halt(reason.clone());
        }
        let now = clock.now_ms();
        if now < self.open_until {
            return Grant::Wait(self.open_until - now);
        }
        if self.last_refill == 0 {
            self.last_refill = now;
            self.day_start = now;
            self.minute_start = now;
        }
        let rate_per_ms = self.config.rpm.max(1) as f64 / 60_000.0;
        self.tokens = (self.tokens + (now - self.last_refill).max(0) as f64 * rate_per_ms)
            .min(self.config.rpm.max(1) as f64);
        self.last_refill = now;
        if now - self.day_start >= DAY_MS {
            self.day_start = now;
            self.day_count = 0;
        }
        if self.config.rpd > 0 && self.day_count >= self.config.rpd {
            return Grant::Wait((self.day_start + DAY_MS - now).max(1));
        }
        if now - self.minute_start >= MINUTE_MS {
            self.minute_start = now;
            self.minute_count = 0;
        }
        if self.config.rpm > 0 && self.minute_count >= self.config.rpm {
            return Grant::Wait((self.minute_start + MINUTE_MS - now).max(1));
        }
        if self.in_flight >= self.config.concurrency.max(1) {
            return Grant::Wait(self.config.min_gap_ms.max(1));
        }
        let since = now - self.last_grant;
        let gap = self.gap();
        if since < gap {
            return Grant::Wait(gap - since);
        }
        if self.tokens < 1.0 {
            let need = 1.0 - self.tokens;
            return Grant::Wait(((need / rate_per_ms).ceil() as i64).max(1));
        }
        self.tokens -= 1.0;
        self.last_grant = now;
        self.day_count += 1;
        self.minute_count += 1;
        self.in_flight += 1;
        self.grants += 1;
        Grant::Allow
    }

    pub fn release(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Feed one call's result to the breaker. A rate-limited failure (429/403 the
    /// adapter mapped) counts toward the consecutive streak; `threshold` in a row
    /// opens the breaker and backs off exponentially, and `max_opens` opens over
    /// the run halts it with a violation reason. A non-rate failure and a success
    /// both clear the streak — the breaker guards the account, not correctness.
    pub fn record(&mut self, now: i64, rate_limited: bool) -> FailureOutcome {
        if !rate_limited {
            self.consecutive_fails = 0;
            return FailureOutcome::default();
        }
        self.consecutive_fails += 1;
        if self.consecutive_fails < self.config.breaker_threshold.max(1) {
            return FailureOutcome::default();
        }
        self.consecutive_fails = 0;
        self.opens += 1;
        let shift = (self.opens - 1).min(20);
        let backoff = self
            .config
            .backoff_base_ms
            .saturating_mul(1_i64 << shift)
            .min(self.config.backoff_max_ms);
        self.open_until = now + backoff;
        let halted = self.opens >= self.config.breaker_max_opens.max(1);
        if halted {
            self.halted = Some(format!(
                "rate-limit circuit breaker opened {} times: the account is being throttled",
                self.opens
            ));
        }
        FailureOutcome {
            opened: true,
            halted,
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_fails = 0;
    }

    pub fn halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    pub fn opens(&self) -> u32 {
        self.opens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct FakeClock(AtomicI64);
    impl FakeClock {
        fn new() -> Self {
            FakeClock(AtomicI64::new(0))
        }
        fn advance(&self, ms: i64) {
            self.0.fetch_add(ms, Ordering::Relaxed);
        }
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn config() -> RateConfig {
        RateConfig {
            rpm: 6,
            rpd: 0,
            min_gap_ms: 0,
            jitter_ms: 0,
            concurrency: 1,
            ..RateConfig::default()
        }
    }

    #[test]
    fn never_exceeds_rpm_over_a_simulated_minute() {
        let clock = FakeClock::new();
        let mut limiter = Limiter::new(config());
        let mut granted = 0u32;
        for _ in 0..100 {
            for _ in 0..1000 {
                match limiter.try_acquire(&clock) {
                    Grant::Allow => {
                        granted += 1;
                        limiter.release();
                    }
                    Grant::Wait(_) => break,
                    Grant::Halt(_) => break,
                }
            }
            clock.advance(600);
        }
        assert!(granted <= 6, "granted {granted} in one simulated minute");
        assert!(granted >= 5, "starved: only {granted}");
    }

    #[test]
    fn min_gap_paces_calls() {
        let clock = FakeClock::new();
        let mut config = config();
        config.rpm = 600;
        config.min_gap_ms = 1000;
        let mut limiter = Limiter::new(config);
        assert_eq!(limiter.try_acquire(&clock), Grant::Allow);
        limiter.release();
        assert!(matches!(limiter.try_acquire(&clock), Grant::Wait(w) if w == 1000));
        clock.advance(1000);
        assert_eq!(limiter.try_acquire(&clock), Grant::Allow);
    }

    #[test]
    fn concurrency_one_blocks_a_second_in_flight() {
        let clock = FakeClock::new();
        let mut limiter = Limiter::new(config());
        assert_eq!(limiter.try_acquire(&clock), Grant::Allow);
        assert!(matches!(limiter.try_acquire(&clock), Grant::Wait(_)));
        limiter.release();
        clock.advance(1);
        assert_eq!(limiter.try_acquire(&clock), Grant::Allow);
    }

    #[test]
    fn breaker_opens_on_consecutive_429s_and_backs_off() {
        let mut limiter = Limiter::new(config());
        assert_eq!(limiter.record(0, true), FailureOutcome::default());
        assert_eq!(limiter.record(0, true), FailureOutcome::default());
        let outcome = limiter.record(0, true);
        assert!(outcome.opened && !outcome.halted);
        let clock = FakeClock::new();
        assert!(matches!(limiter.try_acquire(&clock), Grant::Wait(w) if w > 0));
    }

    #[test]
    fn success_resets_the_streak() {
        let mut limiter = Limiter::new(config());
        limiter.record(0, true);
        limiter.record(0, true);
        limiter.record_success();
        assert_eq!(limiter.record(0, true), FailureOutcome::default());
    }

    #[test]
    fn breaker_halts_after_max_opens_and_emits_violation() {
        let mut config = config();
        config.breaker_threshold = 1;
        config.breaker_max_opens = 3;
        let mut limiter = Limiter::new(config);
        assert!(!limiter.record(0, true).halted);
        assert!(!limiter.record(0, true).halted);
        let outcome = limiter.record(0, true);
        assert!(outcome.halted);
        assert!(limiter.halted().is_some());
        let clock = FakeClock::new();
        assert!(matches!(limiter.try_acquire(&clock), Grant::Halt(_)));
    }
}
