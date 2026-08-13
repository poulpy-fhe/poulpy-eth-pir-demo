use std::collections::HashSet;
use std::time::Duration;

use alloy::primitives::Address;

use super::*;
use crate::follow::watch::ChainWatch;

#[test]
fn backoff_doubles_then_holds_at_the_ceiling() {
    let cfg = FollowConfig::default();
    let mut d = cfg.retry_base;
    let mut seen = vec![d];
    for _ in 0..12 {
        d = loop_run::next_backoff(d, cfg.retry_max);
        seen.push(d);
    }
    assert_eq!(
        &seen[..5],
        &[
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(32),
        ]
    );
    assert_eq!(*seen.last().unwrap(), cfg.retry_max);
    assert!(seen.windows(2).all(|w| w[1] >= w[0]));
}

#[test]
fn backoff_cannot_overflow_past_the_ceiling() {
    assert_eq!(
        loop_run::next_backoff(Duration::MAX, Duration::from_secs(300)),
        Duration::from_secs(300)
    );
    let huge = Duration::MAX / 2 + Duration::from_secs(1);
    assert_eq!(loop_run::next_backoff(huge, Duration::MAX), Duration::MAX);
}

#[test]
fn tip_parses_confirmations_and_finalized() {
    use crate::chain::Tip;
    assert_eq!("finalized".parse(), Ok(Tip::Finalized));
    assert_eq!("FINALIZED".parse(), Ok(Tip::Finalized));
    assert_eq!("32".parse(), Ok(Tip::Confirmations(32)));
    assert_eq!("0".parse(), Ok(Tip::Confirmations(0)));
    assert!("latest".parse::<Tip>().is_err());
    assert!("-1".parse::<Tip>().is_err());
    assert!(!Tip::Finalized.is_reorgable());
    assert!(Tip::Confirmations(64).is_reorgable());
}

#[test]
fn the_watch_keeps_only_what_a_reorg_can_reach() {
    let a = |b: u8| Address::from([b; 20]);
    let mut w = ChainWatch::default();
    w.record(100, vec![a(1)]);
    w.record(150, vec![a(2)]);
    w.record(200, vec![a(3), a(1)]);
    w.trim(200, 64);
    assert_eq!(
        HashSet::<Address>::from_iter(w.addresses()),
        HashSet::from([a(1), a(2), a(3)])
    );
    assert_eq!(w.len(), 2);
    w.trim(200, 10);
    assert_eq!(
        HashSet::<Address>::from_iter(w.addresses()),
        HashSet::from([a(1), a(3)])
    );
}

#[test]
fn empty_batches_are_not_remembered() {
    let mut w = ChainWatch::default();
    w.record(100, vec![]);
    assert_eq!(w.len(), 0);
    assert!(w.addresses().is_empty());
}

#[test]
fn known_provider_cap_messages_are_recognised() {
    assert!(logs::is_result_cap_message(
        "query returned more than 10000 results"
    ));
    assert!(logs::is_result_cap_message(
        "eth_getLogs is limited to 0 - 50 blocks range"
    ));
    assert!(logs::is_result_cap_message("Log response size exceeded"));
    assert!(logs::is_result_cap_message("block range too large"));
    assert!(!logs::is_result_cap_message(
        "Archive requests require a personal token"
    ));
    assert!(!logs::is_result_cap_message("execution reverted"));
    assert!(!logs::is_result_cap_message("rate limit"));
}
