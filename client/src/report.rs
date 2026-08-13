use usdt_pir_record::{Entry, format_units};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenBalance {
    pub symbol: &'static str,
    /// Decimal string, six places.
    pub amount: String,
    /// Base units, as stored.
    pub raw: u128,
    /// Block at which this balance was last re-read, `0` if never.
    pub last_change_block: u32,
}

impl TokenBalance {
    fn new(symbol: &'static str, raw: u128, last_change_block: u32) -> Self {
        Self {
            symbol,
            amount: format_units(raw),
            raw,
            last_change_block,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// EIP-55 checksummed.
    pub address: String,
    /// Whether the server holds a record for this address.
    ///
    /// `false` is a real answer, not a failure: the address holds neither token
    /// in the served set. It is also what a retrieved-but-mismatched record
    /// decodes to, which is why the address prefix is checked before the
    /// payload is trusted.
    pub held: bool,
    pub usdt: TokenBalance,
    pub usdc: TokenBalance,
}

impl Report {
    pub fn found(address: String, entry: Entry) -> Self {
        Self {
            address,
            held: true,
            usdt: TokenBalance::new("USDT", entry.usdt, entry.usdt_block),
            usdc: TokenBalance::new("USDC", entry.usdc, entry.usdc_block),
        }
    }

    pub fn not_held(address: String) -> Self {
        Self {
            address,
            held: false,
            usdt: TokenBalance::new("USDT", 0, 0),
            usdc: TokenBalance::new("USDC", 0, 0),
        }
    }

    /// The most recent block at which either balance was re-read.
    pub fn as_of_block(&self) -> u32 {
        self.usdt.last_change_block.max(self.usdc.last_change_block)
    }

    pub fn to_json(&self) -> String {
        format!(
            concat!(
                r#"{{"address":"{}","held":{},"asOfBlock":{},"#,
                r#""usdt":{{"symbol":"USDT","amount":"{}","raw":"{}","lastChangeBlock":{}}},"#,
                r#""usdc":{{"symbol":"USDC","amount":"{}","raw":"{}","lastChangeBlock":{}}}}}"#
            ),
            self.address,
            self.held,
            self.as_of_block(),
            self.usdt.amount,
            self.usdt.raw,
            self.usdt.last_change_block,
            self.usdc.amount,
            self.usdc.raw,
            self.usdc.last_change_block,
        )
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.address)?;
        if !self.held {
            return write!(
                f,
                "  holds no USDT or USDC in the served set (as of the server's last rebuild)"
            );
        }
        writeln!(
            f,
            "  USDT {:>24}  (last change at block {})",
            self.usdt.amount, self.usdt.last_change_block
        )?;
        write!(
            f,
            "  USDC {:>24}  (last change at block {})",
            self.usdc.amount, self.usdc.last_change_block
        )
    }
}
