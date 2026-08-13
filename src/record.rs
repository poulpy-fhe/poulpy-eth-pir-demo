//! The eth-pir boundary. The record layout itself lives in `usdt-pir-record`,
//! shared with the client.

use alloy::primitives::Address;

pub use usdt_pir_record::UsdtUsdc;

/// An alloy [`Address`] as eth-pir's keyword type.
pub fn keyword(addr: &Address) -> eth_pir::Address {
    addr.into_array()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alloy_address_is_the_eth_pir_keyword() {
        let addr = alloy::primitives::address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        assert_eq!(keyword(&addr), addr.into_array());
    }
}
