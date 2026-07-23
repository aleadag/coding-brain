//! coding-brain-core — foundational types and IO primitives.
//!
//! Carved out from the binary crate as the first step of the workspace
//! refactor (epic #279). The binary, TUI, brain, bus, and every future crate
//! depend on this; this crate depends on nothing Coding Brain-specific in
//! return. Dependency direction is enforced by CI (#277) once the rest of
//! the epic lands.

#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

pub mod brain_activity;
pub mod codex_transcript;
pub mod config;
pub mod discovery;
pub mod health;
pub mod helpers;
pub mod history;
pub mod hooks;
pub mod lifecycle;
pub mod logger;
pub mod models;
pub mod monitor;
pub mod paths;
pub mod process;
pub mod project;
pub mod provider;
pub mod rules;
pub mod runtime;
pub mod session;
pub mod session_links;
pub mod skills;
pub mod terminals;
pub mod theme;
pub mod transcript;

pub use discovery::claude::{ClaudeInventoryCache, ClaudeInventoryEntry};
pub use discovery::{ProviderDiscoveryState, scan_agent_sessions_with_state};
