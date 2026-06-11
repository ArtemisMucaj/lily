//! Runtime configuration, sourced from environment variables.

use anyhow::{Context as _, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Discord bot token (`DISCORD_TOKEN`). Only needed by `lily run`.
    pub discord_token: Option<String>,
    /// Base URL of the OpenCode server (`OPENCODE_URL`).
    pub opencode_url: String,
    /// Data directory for the database and worktrees (`LILY_DATA_DIR`).
    pub data_dir: PathBuf,
    /// How long a message waits behind a running step before lily aborts the
    /// step and force-delivers it (`LILY_INTERRUPT_STEP_TIMEOUT_MS`).
    pub interrupt_timeout_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let data_dir = match std::env::var("LILY_DATA_DIR") {
            Ok(v) => PathBuf::from(v),
            Err(_) => {
                let home = std::env::var("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".lily")
            }
        };
        Ok(Self {
            discord_token: std::env::var("DISCORD_TOKEN").ok(),
            opencode_url: std::env::var("OPENCODE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:4096".to_string()),
            data_dir,
            interrupt_timeout_ms: std::env::var("LILY_INTERRUPT_STEP_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3000),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("lily.db")
    }
}
