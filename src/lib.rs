//! Shared desktop, CLI and MCP application state.
pub mod config;
pub mod engine;
#[cfg(feature = "gui")]
pub mod gui;
pub mod mcp;
pub mod schema;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
