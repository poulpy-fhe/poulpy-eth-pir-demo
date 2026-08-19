use alloy::eips::BlockId;
use alloy::primitives::B256;
use alloy::providers::Provider;
use anyhow::{Context, Result};

pub async fn finalized_block<P: Provider>(provider: &P) -> Result<u64> {
    let block = provider
        .get_block(BlockId::finalized())
        .await?
        .context("node returned no finalized block")?;
    Ok(block.header.number)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tip {
    Finalized,
    Confirmations(u64),
}

impl std::str::FromStr for Tip {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "finalized" => Ok(Tip::Finalized),
            n => {
                let depth = n.parse::<u64>().map_err(|_| {
                    format!("expected a number of confirmations or `finalized`, got `{s}`")
                })?;
                if depth == 0 {
                    return Err("confirmation depth must be a positive integer".into());
                }
                Ok(Tip::Confirmations(depth))
            }
        }
    }
}

impl Tip {
    pub async fn resolve<P: Provider>(self, provider: &P) -> Result<u64> {
        Ok(match self {
            Tip::Finalized => finalized_block(provider).await?,
            Tip::Confirmations(n) => {
                let head = provider.get_block_number().await?;
                confirmed_target(head, n)?
            }
        })
    }

    pub fn is_reorgable(self) -> bool {
        matches!(self, Tip::Confirmations(_))
    }
}

/// Resolve the application confirmation policy. A depth of four means exactly
/// `head - 4`; the head is not its own confirmation.
pub fn confirmed_target(head: u64, confirmations: u64) -> Result<u64> {
    anyhow::ensure!(
        confirmations > 0,
        "confirmation depth must be a positive integer"
    );
    head.checked_sub(confirmations).ok_or_else(|| {
        anyhow::anyhow!(
            "chain head {head} is below confirmation depth {confirmations}; cannot select a target"
        )
    })
}

pub async fn block_hash<P: Provider>(provider: &P, number: u64) -> Result<Option<B256>> {
    Ok(provider
        .get_block(BlockId::number(number))
        .await?
        .map(|b| b.header.hash))
}
