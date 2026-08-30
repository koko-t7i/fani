pub mod agent;
pub mod cli;
pub mod config;
pub mod db;
pub mod github;
pub mod gitout;
pub mod lock;
pub mod markdown;
pub mod matching;
pub mod materialize;
pub mod model;
pub mod orchestrator;
mod process;
pub mod prompts;
pub mod report;
pub mod source;

pub use process::run_process_wrapper_if_requested;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
