#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

// Foundational modules live in coding-brain-core and are re-exported here under
// their original names so root-package and internal `crate::session::*` paths
// continue to resolve. coding-brain-tui depends on Core directly.
pub use coding_brain_core::{
    discovery, health, helpers, history, hooks, logger, models, monitor, process, rules, session,
    skills, terminals, theme, transcript,
};
pub use coding_brain_tui::{brain_app, ui};
pub mod config;

pub mod brain;
pub mod doctor;
pub mod init;
mod lifecycle_hook;
mod provider_hooks;
pub mod runtime;
