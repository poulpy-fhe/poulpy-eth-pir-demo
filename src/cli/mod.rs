mod args;
mod commands;
mod follow_cmd;
mod inspect;
mod serve_cmd;
mod sync_cmd;

pub async fn run() -> anyhow::Result<()> {
    commands::run(args::parse()).await
}
