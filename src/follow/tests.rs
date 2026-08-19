use alloy::primitives::Address;
use std::collections::HashSet;

use super::*;
use crate::follow::watch::ChainWatch;

#[test]
fn tip_parses_confirmations_and_finalized() {
    use crate::chain::Tip;
    assert_eq!("finalized".parse(), Ok(Tip::Finalized));
    assert_eq!("FINALIZED".parse(), Ok(Tip::Finalized));
    assert_eq!("32".parse(), Ok(Tip::Confirmations(32)));
    assert!("0".parse::<Tip>().is_err());
    assert!("latest".parse::<Tip>().is_err());
    assert!("-1".parse::<Tip>().is_err());
    assert!(!Tip::Finalized.is_reorgable());
    assert!(Tip::Confirmations(64).is_reorgable());
}

#[test]
fn numeric_confirmation_targets_are_positive_checked_subtractions() {
    assert_eq!(crate::chain::confirmed_target(100, 4).unwrap(), 96);
    assert!(crate::chain::confirmed_target(100, 0).is_err());
    assert!(crate::chain::confirmed_target(3, 4).is_err());
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
