use alloy::providers::ProviderBuilder;
use anyhow::Result;

use crate::cli::args::Cmd;

pub async fn run(cmd: Cmd) -> Result<()> {
    let Cmd::Bootstrap {
        rpc,
        confirmations,
        state,
        cache,
        chunk,
        retries,
        keep_cache,
    } = cmd
    else {
        unreachable!("bootstrap_cmd only handles bootstrap")
    };
    let outcome = async {
        let provider = ProviderBuilder::new().connect(&rpc).await?;
        crate::bootstrap::run(
            &provider,
            crate::bootstrap::Config {
                state,
                cache,
                confirmations,
                chunk,
                retries,
                keep_cache,
            },
        )
        .await
    }
    .await;
    outcome.map_err(crate::redact::error)
}
