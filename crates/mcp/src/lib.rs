//! Root-session adapter for the official MCP Rust SDK. No SDK types cross the session API.
mod catalog;
mod convert;
mod credential;
mod http_client;
mod manager;
mod resource_tools;
mod transport;

pub mod config;
pub use config::{McpConfig, McpConfigPersister, McpConfigSource};
pub use manager::McpManager;
