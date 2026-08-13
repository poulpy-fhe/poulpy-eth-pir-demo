use std::fmt;

#[derive(Debug)]
pub enum ClientError {
    /// The address is not 20 hex bytes, or fails its EIP-55 checksum.
    BadAddress(String),
    /// `decode` was given an id that `query` never handed out, or one already
    /// consumed.
    UnknownQuery(u32),
    /// The directory blob, tail envelope, or response failed to parse.
    Pir(eth_pir::EthPirError),
}

impl From<eth_pir::EthPirError> for ClientError {
    fn from(e: eth_pir::EthPirError) -> Self {
        Self::Pir(e)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAddress(s) => write!(f, "not a valid Ethereum address: {s}"),
            Self::UnknownQuery(id) => write!(f, "no pending query with id {id}"),
            Self::Pir(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClientError {}
