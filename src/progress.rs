//! How far behind the chain the syncer is, readable from outside the process.
//!
//! Without this a stalled syncer is invisible: `serve` keeps answering queries
//! from the last map it built, so the only symptom of an RPC that died hours ago
//! is balances that quietly stop moving. Publishing the cursor and the tip lets a
//! monitor alarm on it.
//!
//! Atomics rather than a lock: the syncer writes three numbers once per pass and
//! the endpoint reads them per request, so there is nothing to contend over.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Handle = Arc<Progress>;

#[derive(Debug, Default)]
pub struct Progress {
    cursor: AtomicU64,
    tip: AtomicU64,
    /// Unix seconds of the last recorded pass; 0 until the first one.
    at: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub cursor: u64,
    pub tip: u64,
    /// Blocks between the cursor and the tip the syncer was last aiming at.
    pub lag_blocks: u64,
    /// Seconds since the last pass, or `None` if none has completed.
    pub age_secs: Option<u64>,
}

pub fn handle() -> Handle {
    Arc::new(Progress::default())
}

impl Progress {
    pub fn record(&self, cursor: u64, tip: u64) {
        self.cursor.store(cursor, Ordering::Relaxed);
        self.tip.store(tip, Ordering::Relaxed);
        self.at.store(now(), Ordering::Relaxed);
    }

    pub fn sample(&self) -> Sample {
        let cursor = self.cursor.load(Ordering::Relaxed);
        let tip = self.tip.load(Ordering::Relaxed);
        let at = self.at.load(Ordering::Relaxed);
        Sample {
            cursor,
            tip,
            lag_blocks: tip.saturating_sub(cursor),
            // Saturating: a clock that went backwards should read as fresh, not
            // wrap to something enormous and trip an alarm.
            age_secs: (at > 0).then(|| now().saturating_sub(at)),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_recorded_reads_as_unknown_not_as_zero_lag() {
        let s = Progress::default().sample();
        assert_eq!(
            s.age_secs, None,
            "never synced is not the same as just synced"
        );
        assert_eq!(s.lag_blocks, 0);
    }

    #[test]
    fn lag_is_the_gap_to_the_tip() {
        let p = Progress::default();
        p.record(100, 175);
        let s = p.sample();
        assert_eq!((s.cursor, s.tip, s.lag_blocks), (100, 175, 75));
        assert!(s.age_secs.is_some_and(|a| a < 5));
    }

    /// A cursor past the tip happens under numeric confirmations when the tip
    /// moves between resolve and record. It is not negative lag.
    #[test]
    fn a_cursor_past_the_tip_reads_as_caught_up() {
        let p = Progress::default();
        p.record(200, 199);
        assert_eq!(p.sample().lag_blocks, 0);
    }

    #[test]
    fn the_latest_record_wins() {
        let p = Progress::default();
        p.record(100, 150);
        p.record(140, 160);
        let s = p.sample();
        assert_eq!((s.cursor, s.tip, s.lag_blocks), (140, 160, 20));
    }
}
