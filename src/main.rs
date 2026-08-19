//! CLI-based PIR over USDT/USDC balances on Ethereum mainnet.

mod bootstrap;
mod chain;
mod cli;
mod directory;
mod follow;
mod http;
mod keyword_store;
mod map;
mod progress;
mod publish;
mod record;
mod redact;
mod tokens;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "usdt_pir=info".into()),
        )
        .init();

    cli::run().await
}
