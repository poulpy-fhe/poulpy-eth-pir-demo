//! The USDT/USDC PIR record layout, shared by the server that writes records
//! and the client that reads them.
//!
//! eth-pir owns the first 20 bytes of every 64-byte record — the queried address,
//! which the client compares against to prove the record it got back is the one
//! it asked for. A [`RecordCodec`] never sees those bytes; it lays out the
//! remaining [`PAYLOAD_BYTES`] and nothing else.

use eth_pir::{PAYLOAD_BYTES, RecordCodec};

/// Both tokens quote balances in millionths.
pub const DECIMALS: u32 = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub usdt: u128,
    pub usdt_block: u32,
    pub usdc: u128,
    pub usdc_block: u32,
}

impl Entry {
    pub fn is_zero(&self) -> bool {
        self.usdt == 0 && self.usdc == 0
    }
}

/// Both balances and both last-change blocks, little-endian:
///
/// ```text
///   usdt(16) | usdt_blk(4) | usdc(16) | usdc_blk(4) | spare(4)
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsdtUsdc;

const USDT: usize = 0;
const USDT_BLOCK: usize = USDT + 16;
const USDC: usize = USDT_BLOCK + 4;
const USDC_BLOCK: usize = USDC + 16;
const SPARE: usize = USDC_BLOCK + 4;

const _: () = assert!(SPARE <= PAYLOAD_BYTES);

impl RecordCodec for UsdtUsdc {
    type Value = Entry;

    fn encode(e: &Entry) -> [u8; PAYLOAD_BYTES] {
        let mut p = [0u8; PAYLOAD_BYTES];
        p[USDT..USDT_BLOCK].copy_from_slice(&e.usdt.to_le_bytes());
        p[USDT_BLOCK..USDC].copy_from_slice(&e.usdt_block.to_le_bytes());
        p[USDC..USDC_BLOCK].copy_from_slice(&e.usdc.to_le_bytes());
        p[USDC_BLOCK..SPARE].copy_from_slice(&e.usdc_block.to_le_bytes());
        p
    }

    fn decode(p: &[u8; PAYLOAD_BYTES]) -> Entry {
        Entry {
            usdt: u128::from_le_bytes(p[USDT..USDT_BLOCK].try_into().unwrap()),
            usdt_block: u32::from_le_bytes(p[USDT_BLOCK..USDC].try_into().unwrap()),
            usdc: u128::from_le_bytes(p[USDC..USDC_BLOCK].try_into().unwrap()),
            usdc_block: u32::from_le_bytes(p[USDC_BLOCK..SPARE].try_into().unwrap()),
        }
    }
}

/// Render a base-unit balance as a decimal string, e.g. `1234567` -> `1.234567`.
pub fn format_units(v: u128) -> String {
    let scale = 10u128.pow(DECIMALS);
    format!("{}.{:06}", v / scale, v % scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserved_tail_stays_zero() {
        let p = UsdtUsdc::encode(&Entry {
            usdt: u128::MAX,
            usdt_block: u32::MAX,
            usdc: u128::MAX,
            usdc_block: u32::MAX,
        });
        assert_eq!(
            &p[SPARE..],
            &[0u8; PAYLOAD_BYTES - SPARE],
            "reserved tail must stay zero even at saturation"
        );
    }

    #[test]
    fn entries_round_trip_through_a_payload() {
        for e in [
            Entry::default(),
            Entry {
                usdt: 1_000_000,
                usdt_block: 21_000_000,
                usdc: 0,
                usdc_block: 0,
            },
            Entry {
                usdt: 0,
                usdt_block: 0,
                usdc: 2_500_000,
                usdc_block: 20_999_999,
            },
            Entry {
                usdt: u128::MAX,
                usdt_block: u32::MAX,
                usdc: u128::MAX,
                usdc_block: u32::MAX,
            },
        ] {
            assert_eq!(UsdtUsdc::decode(&UsdtUsdc::encode(&e)), e);
        }
    }

    /// A balance bleeding into a block stamp would decode as a plausible number
    /// rather than failing.
    #[test]
    fn fields_do_not_overlap() {
        let only_usdt = UsdtUsdc::decode(&UsdtUsdc::encode(&Entry {
            usdt: u128::MAX,
            ..Default::default()
        }));
        assert_eq!(only_usdt.usdt, u128::MAX);
        assert_eq!(
            (only_usdt.usdt_block, only_usdt.usdc, only_usdt.usdc_block),
            (0, 0, 0)
        );

        let only_usdc_block = UsdtUsdc::decode(&UsdtUsdc::encode(&Entry {
            usdc_block: u32::MAX,
            ..Default::default()
        }));
        assert_eq!(only_usdc_block.usdc_block, u32::MAX);
        assert_eq!(
            (
                only_usdc_block.usdt,
                only_usdc_block.usdt_block,
                only_usdc_block.usdc
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn balances_render_with_six_decimals() {
        assert_eq!(format_units(0), "0.000000");
        assert_eq!(format_units(1_234_567), "1.234567");
        assert_eq!(format_units(1), "0.000001");
    }
}
