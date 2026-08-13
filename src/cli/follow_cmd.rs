use alloy::providers::ProviderBuilder;
use anyhow::Result;

use crate::cli::args::Cmd;

pub async fn run(cmd: Cmd) -> Result<()> {
    let Cmd::Follow {
        rpc,
        state,
        from_block,
        chunk,
        snapshot_every,
        confirmations,
        reorg_window,
        poll_interval,
    } = cmd
    else {
        unreachable!("follow_cmd only handles follow")
    };
    let provider = ProviderBuilder::new().connect(&rpc).await?;
    let _lock = crate::map::SnapshotLock::acquire(&state)?;
    let mut balances = super::commands::open_map(&provider, &state, from_block).await?;
    crate::follow::preflight(&provider, balances.cursor + 1).await?;
    super::commands::log_numeric_confirmations(confirmations, reorg_window);
    let cfg = super::commands::follow_config(
        chunk,
        state,
        snapshot_every,
        confirmations,
        reorg_window,
        poll_interval,
    );
    crate::follow::run(&provider, &mut balances, &cfg, None).await
}
