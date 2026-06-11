//! Connector layer: adapters to the outside world.
//!
//! Discord gateway, OpenCode HTTP server, SQLite persistence, and git
//! plumbing. Connectors translate between external systems and domain types.

pub mod discord;
pub mod git;
pub mod opencode;
pub mod sqlite;
