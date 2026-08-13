use super::*;

const VITALIK: &str = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";

#[test]
fn addresses_parse_in_any_uniform_case() {
    let lower = VITALIK.to_lowercase();
    let upper = format!("0x{}", VITALIK[2..].to_uppercase());
    let expected = parse_address(VITALIK).unwrap();
    assert_eq!(parse_address(&lower).unwrap(), expected);
    assert_eq!(parse_address(&upper).unwrap(), expected);
    assert_eq!(parse_address(&lower[2..]).unwrap(), expected, "0x optional");
}

/// A single flipped case bit in a mixed-case address is exactly what EIP-55
/// exists to catch, so it must not be accepted.
#[test]
fn a_broken_eip55_checksum_is_rejected() {
    let mut bad: Vec<char> = VITALIK.chars().collect();
    let letter = (2..bad.len())
        .find(|&i| bad[i].is_ascii_alphabetic())
        .expect("address has a letter to flip");
    bad[letter] = if bad[letter].is_ascii_uppercase() {
        bad[letter].to_ascii_lowercase()
    } else {
        bad[letter].to_ascii_uppercase()
    };
    let bad: String = bad.into_iter().collect();
    assert!(matches!(
        parse_address(&bad),
        Err(ClientError::BadAddress(_))
    ));
}

#[test]
fn malformed_addresses_are_rejected() {
    for s in [
        "",
        "0x",
        "0xnothex0000000000000000000000000000000000",
        "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA960", // too short
        "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA9604500", // too long
    ] {
        assert!(
            matches!(parse_address(s), Err(ClientError::BadAddress(_))),
            "should have rejected {s:?}"
        );
    }
}

#[test]
fn a_report_renders_both_tokens_and_its_json() {
    let r = Report::found(
        VITALIK.to_string(),
        usdt_pir_record::Entry {
            usdt: 1_234_567,
            usdt_block: 21_000_000,
            usdc: 0,
            usdc_block: 0,
        },
    );
    assert_eq!(r.usdt.amount, "1.234567");
    assert_eq!(r.usdc.amount, "0.000000");
    assert_eq!(r.as_of_block(), 21_000_000);

    let json = r.to_json();
    assert!(json.contains(r#""held":true"#), "{json}");
    assert!(json.contains(r#""amount":"1.234567""#), "{json}");
    assert!(json.contains(r#""asOfBlock":21000000"#), "{json}");
    // u128 as a JSON string: past 2^53 a number literal would lose precision.
    assert!(json.contains(r#""raw":"1234567""#), "{json}");
}

#[test]
fn a_not_held_report_is_an_answer_not_an_error() {
    let r = Report::not_held(VITALIK.to_string());
    assert!(!r.held);
    assert_eq!(r.usdt.raw, 0);
    assert!(r.to_json().contains(r#""held":false"#));
    assert!(format!("{r}").contains("holds no USDT or USDC"));
}
