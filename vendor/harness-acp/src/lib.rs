//! Application-owned tasks executed by an isolated Goose ACP subprocess.
//! The launcher boundary can also carry a future VM-backed connection.

mod client;
pub mod mcp;
mod process;
mod types;

pub use client::{Session, run, run_observed, run_with_launcher};
pub use process::{Connection, Launcher, ProcessLauncher, Transport};
pub use types::*;
