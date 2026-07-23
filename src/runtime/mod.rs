//! Binary-side implementation of the Coding Brain runtime contract.

use std::sync::Arc;

use coding_brain_core::runtime::BrainRuntime;

mod brain;
mod navigation;

pub use brain::{LiveBrainActions, LiveBrainSource};
pub use navigation::LiveSessionNavigation;

pub fn build_brain_runtime() -> BrainRuntime {
    BrainRuntime::new(
        Arc::new(LiveBrainSource::default()),
        Arc::new(LiveBrainActions::default()),
    )
    .with_navigation(Arc::new(LiveSessionNavigation::default()))
}
