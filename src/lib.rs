pub mod agent;
pub mod cli;
pub mod config;
pub mod db;
pub mod gitout;
pub mod lock;
pub mod model;
pub mod orchestrator;
mod process;
pub use process::run_process_wrapper_if_requested;
pub mod report;
pub mod skill;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
