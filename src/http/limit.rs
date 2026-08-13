//! Per-client rate limiting for the query endpoint.
//!
//! A query costs the server a share of a database pass, so a single client in a
//! loop can consume the whole service. The batch queue already bounds *total*
//! work in flight; this bounds any one client's share of it, which the queue
//! alone cannot do — a fast client would simply win the race for every slot.
//!
//! A token bucket rather than a fixed window: it allows a short burst (a page
//! opening several lookups at once) while holding the long-run rate, and it has
//! no edge where a client gets a double allowance across a boundary.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    /// Sustained queries per minute per client. Zero disables limiting.
    pub per_minute: u32,
    /// Queries a client may issue back-to-back before the rate binds.
    pub burst: u32,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            per_minute: 60,
            burst: 10,
        }
    }
}

impl RateLimit {
    fn tokens_per_sec(&self) -> f64 {
        f64::from(self.per_minute) / 60.0
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    limit: RateLimit,
    clients: Mutex<HashMap<IpAddr, Bucket>>,
}

impl RateLimiter {
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.limit.per_minute > 0
    }

    /// Spend one token for `client`, or report how long until one exists.
    pub fn check(&self, client: IpAddr) -> Result<(), Duration> {
        self.check_at(client, Instant::now())
    }

    fn check_at(&self, client: IpAddr, now: Instant) -> Result<(), Duration> {
        if !self.enabled() {
            return Ok(());
        }
        let burst = f64::from(self.limit.burst.max(1));
        let rate = self.limit.tokens_per_sec();

        let Ok(mut clients) = self.clients.lock() else {
            // A poisoned lock must not become a free pass; refuse instead.
            return Err(Duration::from_secs(1));
        };

        make_room(&mut clients, client, now)?;

        let bucket = clients.entry(client).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        let refill = now.duration_since(bucket.last).as_secs_f64() * rate;
        bucket.tokens = (bucket.tokens + refill).min(burst);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let missing = 1.0 - bucket.tokens;
        Err(Duration::from_secs_f64(
            missing / rate.max(f64::MIN_POSITIVE),
        ))
    }
}

const MAX_TRACKED: usize = 100_000;
const IDLE_EVICT: Duration = Duration::from_secs(600);

fn make_room(
    clients: &mut HashMap<IpAddr, Bucket>,
    client: IpAddr,
    now: Instant,
) -> Result<(), Duration> {
    if clients.contains_key(&client) || clients.len() < MAX_TRACKED {
        return Ok(());
    }
    clients.retain(|_, b| now.duration_since(b.last) < IDLE_EVICT);
    if clients.len() < MAX_TRACKED {
        return Ok(());
    }
    Err(Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

    fn limiter(per_minute: u32, burst: u32) -> RateLimiter {
        RateLimiter::new(RateLimit { per_minute, burst })
    }

    #[test]
    fn a_burst_is_allowed_then_the_rate_binds() {
        let l = limiter(60, 3);
        let t = Instant::now();
        for i in 0..3 {
            assert!(l.check_at(IP, t).is_ok(), "burst token {i} should pass");
        }
        assert!(l.check_at(IP, t).is_err(), "the fourth exceeds the burst");
    }

    #[test]
    fn tokens_refill_over_time() {
        let l = limiter(60, 1); // one per second
        let t = Instant::now();
        assert!(l.check_at(IP, t).is_ok());
        assert!(l.check_at(IP, t).is_err());
        assert!(
            l.check_at(IP, t + Duration::from_secs(1)).is_ok(),
            "a second later there should be a token"
        );
    }

    #[test]
    fn the_wait_it_reports_is_long_enough() {
        let l = limiter(60, 1);
        let t = Instant::now();
        assert!(l.check_at(IP, t).is_ok());
        let wait = l.check_at(IP, t).expect_err("should be limited");
        assert!(
            l.check_at(IP, t + wait).is_ok(),
            "waiting the advertised {wait:?} must actually be enough"
        );
    }

    /// One noisy client must not spend another's allowance.
    #[test]
    fn clients_are_limited_independently() {
        let l = limiter(60, 2);
        let t = Instant::now();
        assert!(l.check_at(IP, t).is_ok());
        assert!(l.check_at(IP, t).is_ok());
        assert!(l.check_at(IP, t).is_err());
        assert!(
            l.check_at(OTHER, t).is_ok(),
            "a different client is unaffected"
        );
    }

    #[test]
    fn zero_disables_limiting() {
        let l = limiter(0, 1);
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(l.check_at(IP, t).is_ok());
        }
        assert!(!l.enabled());
    }

    /// Bursting cannot bank credit beyond the burst size, however long a client
    /// has been idle.
    #[test]
    fn idle_time_does_not_bank_unlimited_credit() {
        let l = limiter(60, 5);
        let t = Instant::now();
        let much_later = t + Duration::from_secs(3600);
        for _ in 0..5 {
            assert!(l.check_at(IP, much_later).is_ok());
        }
        assert!(l.check_at(IP, much_later).is_err(), "capped at the burst");
    }

    #[test]
    fn too_many_fresh_clients_are_refused_instead_of_leaking_memory() {
        let mut clients = HashMap::new();
        let now = Instant::now();
        for i in 0..MAX_TRACKED {
            clients.insert(
                IpAddr::V4(std::net::Ipv4Addr::from(i as u32)),
                Bucket {
                    tokens: 1.0,
                    last: now,
                },
            );
        }
        assert!(make_room(&mut clients, OTHER, now).is_err());
        assert_eq!(clients.len(), MAX_TRACKED);
    }
}
