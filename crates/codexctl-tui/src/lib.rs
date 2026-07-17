//! codexctl-tui — terminal UI, dashboard recording, and demo fixtures.
//!
//! Carved out from the binary crate as part of the workspace refactor
//! (epic #279, issue #275). Owns:
//!
//! * `app` — the main `App` state struct and refresh / input-handling
//!   methods (3k+ lines)
//! * `ui` — render modules (table, detail, help, status_bar, skills,
//!   mod). The binary keeps `brain_screen` because it depends
//!   on `brain::metrics` and `brain::risk` (binary-only modules).
//! * `recorder`, `session_recorder`, `demo` — peripherals
//!
//! Depends on `codexctl-core` for foundational types and the runtime
//! trait contract and does not depend on the binary crate.

#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error,
    clippy::too_many_arguments
)]

pub mod app;
pub mod brain_app;
pub mod demo;
pub mod recorder;
pub mod session_recorder;
pub mod ui;
