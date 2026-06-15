pub mod cli;
pub mod config;
pub mod daemon;
pub mod outcome;
pub mod policy;
pub mod prompt;
pub mod sources;
pub mod store;
pub mod submit;
pub mod worktree;

pub type LoopResult<T> = Result<T, String>;
