//! lily — a collaborative agent orchestrator inside Discord.
//!
//! Channels are projects, threads are coding sessions. lily bridges Discord
//! to a local OpenCode server: send a message in a project channel and an AI
//! agent edits code on this machine, replying in a thread.
//!
//! The crate is layered domain-driven-design style:
//! - `domain`      pure business rules (no I/O)
//! - `application` use cases orchestrating domain rules over connectors
//! - `connector`   adapters: Discord, OpenCode, SQLite, git
//! - `cli`         command-line entry points

mod application;
mod cli;
mod connector;
mod domain;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,serenity=warn".into()),
        )
        .init();
    cli::run().await
}
