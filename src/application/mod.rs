//! Application layer: use cases orchestrating domain rules over connectors.
//!
//! The session runtime drives prompts through a thread's queue (with the
//! interrupt grace period), and the task runner executes scheduled tasks.

pub mod chat;
pub mod config;
pub mod session_runtime;
pub mod task_runner;
