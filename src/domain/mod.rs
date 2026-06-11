//! Domain layer: pure business rules with no I/O.
//!
//! Everything here is deterministic and unit-testable — message delivery
//! semantics, rendering rules, worktree naming, task scheduling — and is
//! depended on by the application and connector layers, never the reverse.

pub mod delivery;
pub mod rendering;
pub mod session;
pub mod task;
pub mod worktree;
