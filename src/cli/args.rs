use std::path::PathBuf;

use alloy::primitives::Address;
use clap::{Parser, Subcommand};

/// Where the map is persisted unless `--state` says otherwise.
pub const DEFAULT_STATE: &str = "data/balances.snapshot";

/// Base path for the persisted keyword index; `.index` and `.keys` hang off it.
pub const DEFAULT_KEYWORD: &str = "data/keyword";

#[derive(Parser)]
#[command(name = "usdt-pir", about, version)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

pub fn parse() -> Cli {
    Cli::parse()
}

/// Where a sync should stop.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    Finalized,
    Latest,
    Number(u64),
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "finalized" => Ok(Target::Finalized),
            "latest" | "head" => Ok(Target::Latest),
            n => n.parse().map(Target::Number).map_err(|_| {
                format!("expected a block number, `finalized`, or `latest`, got `{s}`")
            }),
        }
    }
}

impl Target {
    pub async fn resolve<P: alloy::providers::Provider>(self, p: &P) -> anyhow::Result<u64> {
        Ok(match self {
            Target::Finalized => crate::chain::finalized_block(p).await?,
            Target::Latest => {
                tracing::warn!(
                    "syncing to `latest`: balances read from unfinalized blocks are \
                     permanently wrong if those blocks are reorged away"
                );
                p.get_block_number().await?
            }
            Target::Number(n) => n,
        })
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Discover every historical USDT/USDC candidate and build one complete snapshot.
    Bootstrap {
        #[arg(long, env = "ETH_RPC_URL", hide_env_values = true)]
        rpc: String,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..))]
        confirmations: u64,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
        /// Intermediate crash-recovery cache. Defaults to `<state>.bootstrap.sqlite`.
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..))]
        chunk: u64,
        /// Additional attempts after the initial failure of one stalled unit.
        #[arg(long, default_value_t = 10)]
        retries: u32,
        /// Keep a completed SQLite cache for diagnostics.
        #[arg(long)]
        keep_cache: bool,
    },
    /// Atomically install a staged, checksummed snapshot for `serve`.
    InstallSnapshot {
        /// Staged snapshot file (it may be on another filesystem).
        #[arg(long)]
        source: PathBuf,
        /// Final serving-state destination.
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
    },
    /// Follow the chain, folding every balance-moving event into the map.
    Follow {
        #[arg(long, env = "ETH_RPC_URL", hide_env_values = true)]
        rpc: String,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
        #[arg(long)]
        from_block: Option<Target>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..))]
        chunk: u64,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        snapshot_every: u64,
        #[arg(long, default_value = "finalized")]
        confirmations: crate::chain::Tip,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
        reorg_window: u64,
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval: u64,
    },
    /// Sync a bounded block range into the map, then exit.
    Sync {
        #[arg(long, env = "ETH_RPC_URL", hide_env_values = true)]
        rpc: String,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
        #[arg(long)]
        from: Option<u64>,
        #[arg(long, default_value = "finalized")]
        to: Target,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..))]
        chunk: u64,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        snapshot_every: u64,
        /// Retries on a failed range before giving up. Progress is kept either
        /// way, so re-running resumes.
        #[arg(long, default_value_t = 10)]
        retries: u32,
    },
    /// Follow the chain and keep a PIR database current from it.
    Serve {
        #[arg(long, env = "ETH_RPC_URL", hide_env_values = true)]
        rpc: String,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
        #[arg(long)]
        from_block: Option<Target>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..))]
        chunk: u64,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        snapshot_every: u64,
        #[arg(long, default_value = "4")]
        confirmations: crate::chain::Tip,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
        reorg_window: u64,
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u64).range(1..))]
        poll_interval: u64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        rebuild_every: u64,
        #[arg(long, default_value_t = 200_000)]
        compact_after: usize,
        /// Compact when the tail download reaches this percentage of the MPHF
        /// blob. 0 disables size-based compaction.
        #[arg(long, default_value_t = 100)]
        compact_tail_percent: usize,
        /// Base path for the persisted keyword index (`.index` and `.keys`).
        /// Keeping it lets a restart preserve every client's slots.
        #[arg(long, default_value = DEFAULT_KEYWORD)]
        keyword: PathBuf,
        /// Serve queries on this address. Omit to run the syncer with no endpoint.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Also serve static files from this directory, for the demo page.
        #[arg(long, requires = "listen")]
        web: Option<PathBuf>,
        /// Milliseconds to pool queries before answering them in one database
        /// pass. 0 answers each query on its own.
        #[arg(long, default_value_t = 1000)]
        batch_window: u64,
        /// Most queries in one pass. Bigger is more throughput and more latency
        /// and memory; eth-pir measures 33.7 q/s at 64 against 8.7 unbatched.
        #[arg(long, default_value_t = 64)]
        max_batch: usize,
        /// Queries allowed to wait before new ones get 503.
        #[arg(long, default_value_t = 256)]
        queue_depth: usize,
        /// Sustained queries per minute per client IP. 0 disables limiting.
        #[arg(long, default_value_t = 60)]
        rate_limit: u32,
        /// Queries one client may issue back-to-back before the rate binds.
        #[arg(long, default_value_t = 10)]
        rate_burst: u32,
    },
    /// Print what the map currently holds for one address.
    Lookup {
        address: Address,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
    },
    /// Summarise the map.
    Stat {
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
    },
    /// Print addresses the map actually holds, for testing a client against.
    Sample {
        #[arg(long, short = 'n', default_value_t = 5)]
        count: usize,
        /// Only addresses holding at least this many whole tokens of either.
        #[arg(long, default_value_t = 1)]
        min: u128,
        #[arg(long, default_value = DEFAULT_STATE)]
        state: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn bootstrap_defaults_are_one_shot_defaults() {
        let cli =
            Cli::try_parse_from(["usdt-pir", "bootstrap", "--rpc", "http://localhost"]).unwrap();
        let Cmd::Bootstrap {
            confirmations,
            state,
            cache,
            chunk,
            retries,
            keep_cache,
            ..
        } = cli.cmd
        else {
            panic!("wrong command")
        };
        assert_eq!(confirmations, 4);
        assert_eq!(state, PathBuf::from(DEFAULT_STATE));
        assert_eq!(cache, None);
        assert_eq!(chunk, 10_000);
        assert_eq!(retries, 10);
        assert!(!keep_cache);
    }

    #[test]
    fn bootstrap_refuses_zero_confirmation_depth() {
        assert!(
            Cli::try_parse_from([
                "usdt-pir",
                "bootstrap",
                "--rpc",
                "http://localhost",
                "--confirmations",
                "0",
            ])
            .is_err()
        );
    }

    #[test]
    fn serve_numeric_confirmation_default_is_four() {
        let cli = Cli::try_parse_from(["usdt-pir", "serve", "--rpc", "http://localhost"]).unwrap();
        let Cmd::Serve { confirmations, .. } = cli.cmd else {
            panic!("wrong command")
        };
        assert_eq!(confirmations, crate::chain::Tip::Confirmations(4));
    }

    #[test]
    fn help_never_renders_rpc_environment_values() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("bootstrap")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("ETH_RPC_URL"));
        assert!(!help.contains("ETH_RPC_URL="));
    }
}
