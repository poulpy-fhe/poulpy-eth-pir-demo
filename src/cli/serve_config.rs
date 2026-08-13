use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::args::Cmd;
use crate::follow::FollowConfig;

pub(super) struct ServeArgs {
    pub rpc: String,
    pub state: PathBuf,
    pub from_block: Option<crate::cli::args::Target>,
    pub follow: FollowArgs,
    pub serving: Serving,
}

pub(super) struct FollowArgs {
    chunk: u64,
    snapshot_every: u64,
    confirmations: crate::chain::Tip,
    reorg_window: u64,
    poll_interval: u64,
}

pub(super) struct Serving {
    pub rebuild_every: u64,
    pub compact_after: usize,
    pub compact_tail_percent: usize,
    pub keyword: PathBuf,
    pub listen: Option<std::net::SocketAddr>,
    pub web: Option<PathBuf>,
    pub batch: crate::http::BatchConfig,
    pub rate: crate::http::RateLimit,
}

struct ServeFields {
    rpc: String,
    state: PathBuf,
    from_block: Option<crate::cli::args::Target>,
    chunk: u64,
    snapshot_every: u64,
    confirmations: crate::chain::Tip,
    reorg_window: u64,
    poll_interval: u64,
    rebuild_every: u64,
    compact_after: usize,
    compact_tail_percent: usize,
    keyword: PathBuf,
    listen: Option<std::net::SocketAddr>,
    web: Option<PathBuf>,
    batch_window: u64,
    max_batch: usize,
    queue_depth: usize,
    rate_limit: u32,
    rate_burst: u32,
}

impl ServeArgs {
    pub fn take(cmd: Cmd) -> Self {
        ServeFields::take(cmd).into()
    }
}

impl ServeFields {
    fn take(cmd: Cmd) -> Self {
        let Cmd::Serve {
            rpc,
            state,
            from_block,
            chunk,
            snapshot_every,
            confirmations,
            reorg_window,
            poll_interval,
            rebuild_every,
            compact_after,
            compact_tail_percent,
            keyword,
            listen,
            web,
            batch_window,
            max_batch,
            queue_depth,
            rate_limit,
            rate_burst,
        } = cmd
        else {
            unreachable!("serve_cmd only handles serve")
        };
        Self {
            rpc,
            state,
            from_block,
            chunk,
            snapshot_every,
            confirmations,
            reorg_window,
            poll_interval,
            rebuild_every,
            compact_after,
            compact_tail_percent,
            keyword,
            listen,
            web,
            batch_window,
            max_batch,
            queue_depth,
            rate_limit,
            rate_burst,
        }
    }
}

impl From<ServeFields> for ServeArgs {
    fn from(fields: ServeFields) -> Self {
        Self {
            rpc: fields.rpc,
            state: fields.state,
            from_block: fields.from_block,
            follow: FollowArgs::new(
                fields.chunk,
                fields.snapshot_every,
                fields.confirmations,
                fields.reorg_window,
                fields.poll_interval,
            ),
            serving: Serving::new(
                fields.rebuild_every,
                fields.compact_after,
                fields.compact_tail_percent,
                fields.keyword,
                fields.listen,
                fields.web,
                fields.batch_window,
                fields.max_batch,
                fields.queue_depth,
                fields.rate_limit,
                fields.rate_burst,
            ),
        }
    }
}

impl FollowArgs {
    fn new(
        chunk: u64,
        snapshot_every: u64,
        confirmations: crate::chain::Tip,
        reorg_window: u64,
        poll_interval: u64,
    ) -> Self {
        Self {
            chunk,
            snapshot_every,
            confirmations,
            reorg_window,
            poll_interval,
        }
    }
}

impl Serving {
    #[allow(clippy::too_many_arguments)]
    fn new(
        rebuild_every: u64,
        compact_after: usize,
        compact_tail_percent: usize,
        keyword: PathBuf,
        listen: Option<std::net::SocketAddr>,
        web: Option<PathBuf>,
        batch_window: u64,
        max_batch: usize,
        queue_depth: usize,
        rate_limit: u32,
        rate_burst: u32,
    ) -> Self {
        Self {
            rebuild_every,
            compact_after,
            compact_tail_percent,
            keyword,
            listen,
            web,
            batch: crate::http::BatchConfig {
                window: Duration::from_millis(batch_window),
                max: max_batch,
                queue_depth,
            },
            rate: crate::http::RateLimit {
                per_minute: rate_limit,
                burst: rate_burst,
            },
        }
    }
}

pub(super) fn progress_config(
    follow: &FollowArgs,
    state: PathBuf,
) -> (FollowConfig, crate::progress::Handle) {
    let mut cfg = super::commands::follow_config(
        follow.chunk,
        state,
        follow.snapshot_every,
        follow.confirmations,
        follow.reorg_window,
        follow.poll_interval,
    );
    let progress = crate::progress::handle();
    cfg.progress = Some(progress.clone());
    (cfg, progress)
}

pub(super) fn pir_config(
    rebuild_every: u64,
    compact_after: usize,
    compact_tail_percent: usize,
    keyword: &Path,
) -> crate::publish::PirConfig {
    crate::publish::PirConfig {
        rebuild_every: Duration::from_secs(rebuild_every),
        compact_after,
        compact_tail_percent,
        keyword: crate::keyword_store::Paths::new(keyword),
    }
}
