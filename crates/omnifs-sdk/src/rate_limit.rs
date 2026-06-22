//! Per-authority rate-limit circuit breaker.
//!
//! One breaker per provider instance, shared by every HTTP send path in this
//! crate (raw `cx.http()`, typed endpoints, and blob fetches). A 429 arms a
//! cooldown window for the request URL's authority, sized by `Retry-After`
//! when present, else by the endpoint's
//! [`crate::endpoint::RateLimitPolicy`], else by a small default; while the
//! window is open, sends to that authority fail in-guest with a
//! rate-limited error carrying the remaining cooldown, without issuing a
//! callout. The first response with status < 400 disarms the authority, and
//! the whole breaker is cleared on provider shutdown so a window never
//! leaks across instance teardown.
//!
//! Providers do not drive this module directly; the send paths do. The one
//! provider-facing hook is [`note_rate_limited`] (re-exported at the crate
//! root) for upstream-specific throttle signals the generic 429 path cannot
//! see, such as GitHub's `403` with `x-ratelimit-remaining: 0`.

use core::time::Duration;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;
use std::time::Instant;

use hashbrown::HashMap;

// 429 cooldown when the upstream sent no Retry-After. Placeholder until a
// per-endpoint #[endpoint(rate_limit = ...)] policy makes this configurable.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5);

// Upper bound on a single breaker window. Guards against an upstream
// `Retry-After` large enough to overflow `Instant::now() + cooldown` (which
// would panic the guest) or wedge the breaker open indefinitely. One hour is
// well past any real backoff for a filesystem.
const MAX_COOLDOWN: Duration = Duration::from_hours(1);

type BreakerMap = HashMap<String, Instant, BuildHasherDefault<DefaultHasher>>;

/// Map of authority to the instant its 429 window closes. Single-threaded
/// (`RefCell`) because provider guests run one operation at a time; access
/// goes through the thread-local in [`with_breaker`].
pub struct RateLimitBreaker {
    open: RefCell<BreakerMap>,
}

impl RateLimitBreaker {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: RefCell::new(HashMap::with_hasher(BuildHasherDefault::new())),
        }
    }

    /// Remaining cooldown if the authority is currently throttled, else None.
    /// Lazily evicts an expired entry so the map cannot grow without bound.
    pub fn check(&self, authority: &str) -> Option<Duration> {
        let now = Instant::now();
        let open = self.open.borrow();
        let open_until = open.get(authority).copied()?;
        if open_until > now {
            return Some(open_until.duration_since(now));
        }
        drop(open);
        self.open.borrow_mut().remove(authority);
        None
    }

    /// Arm after a 429. `retry_after` is the parsed Retry-After (or the
    /// endpoint's policy cooldown) if present; without it the default
    /// cooldown applies. The window is clamped so an absurd upstream value
    /// can neither overflow `Instant` arithmetic nor wedge the breaker open.
    pub fn record_429(&self, authority: &str, retry_after: Option<Duration>) {
        let cooldown = retry_after.unwrap_or(DEFAULT_COOLDOWN).min(MAX_COOLDOWN);
        self.open
            .borrow_mut()
            .insert(authority.to_string(), Instant::now() + cooldown);
    }

    /// Close after a successful (status < 400) response.
    pub fn record_success(&self, authority: &str) {
        self.open.borrow_mut().remove(authority);
    }

    pub fn clear(&self) {
        self.open.borrow_mut().clear();
    }
}

impl Default for RateLimitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static BREAKER: RateLimitBreaker = const { RateLimitBreaker::new() };
}

/// Run `f` with the per-instance breaker. The breaker is SDK-owned (not a
/// macro-emitted per-provider thread-local) because the HTTP/endpoint send
/// path in this crate must reach it.
pub fn with_breaker<R>(f: impl FnOnce(&RateLimitBreaker) -> R) -> R {
    BREAKER.with(f)
}

/// Reset the breaker. Called from the provider macro's `shutdown()` so a 429
/// window does not leak across instance teardown.
pub fn clear_breaker() {
    BREAKER.with(RateLimitBreaker::clear);
}

/// Provider-facing hook to arm the breaker after the provider itself detected
/// a rate limit the generic 429 path would miss, such as github's
/// `403 + x-ratelimit-remaining: 0`. Keyed by the request URL's authority.
pub fn note_rate_limited(url: &str, retry_after: Option<Duration>) {
    if let Some(authority) = authority_of(url) {
        with_breaker(|b| b.record_429(&authority, retry_after));
    }
}

/// Endpoint authority (`scheme://host[:port]`) used as the breaker key.
/// Returns None for an unparseable URL (caller then skips breaker logic).
pub fn authority_of(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?;
    let mut authority = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    Some(authority)
}

/// Parse an HTTP `Retry-After` value's delta-seconds form into a Duration.
/// An HTTP-date form returns None (no guest wall-clock math on the error path).
pub fn parse_retry_after_secs(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_breaker_reports_remaining() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://a.test";

        with_breaker(|b| b.record_429(auth, Some(Duration::from_secs(3))));
        let remaining = with_breaker(|b| b.check(auth)).expect("breaker should be open");

        assert!(remaining <= Duration::from_secs(3));
        assert!(remaining > Duration::from_secs(2));
    }

    #[test]
    fn success_closes_the_breaker() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://success-closes.test";

        with_breaker(|b| {
            b.record_429(auth, Some(Duration::from_secs(3)));
            b.record_success(auth);
        });

        assert_eq!(with_breaker(|b| b.check(auth)), None);
    }

    #[test]
    fn missing_retry_after_uses_default_cooldown() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://missing-retry-after.test";

        with_breaker(|b| b.record_429(auth, None));
        let remaining = with_breaker(|b| b.check(auth)).expect("breaker should be open");

        assert!(remaining <= DEFAULT_COOLDOWN);
    }

    #[test]
    fn huge_retry_after_is_clamped_not_panicking() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://huge-retry.test";

        // `u64::MAX` seconds would overflow `Instant::now() + Duration` without
        // the `MAX_COOLDOWN` clamp; this asserts the clamp and the no-panic.
        with_breaker(|b| b.record_429(auth, Some(Duration::from_secs(u64::MAX))));
        let remaining = with_breaker(|b| b.check(auth)).expect("breaker should be open");

        assert!(remaining <= MAX_COOLDOWN);
    }

    #[test]
    fn authority_keys_isolate_hosts() {
        with_breaker(RateLimitBreaker::clear);
        let auth_a = "https://a-isolated.test";
        let auth_b = "https://b-isolated.test";

        with_breaker(|b| b.record_429(auth_a, Some(Duration::from_secs(3))));

        assert_eq!(with_breaker(|b| b.check(auth_b)), None);
    }

    #[test]
    fn note_rate_limited_arms_breaker() {
        with_breaker(RateLimitBreaker::clear);

        note_rate_limited("https://hook.test/x", Some(Duration::from_secs(3)));
        let remaining =
            with_breaker(|b| b.check("https://hook.test")).expect("breaker should be open");

        assert!(remaining <= Duration::from_secs(3));
    }

    #[test]
    fn authority_of_includes_explicit_port_only() {
        assert_eq!(
            authority_of("https://x.test/p?q=1"),
            Some("https://x.test".into())
        );
        assert_eq!(
            authority_of("http://x.test:8080/p"),
            Some("http://x.test:8080".into())
        );
    }

    #[test]
    fn parse_retry_after_secs_handles_int_and_rejects_date() {
        assert_eq!(parse_retry_after_secs("3"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after_secs("Wed, 21 Oct 2099 07:28:00 GMT"),
            None
        );
    }
}
