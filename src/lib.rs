//! Library for augmcp: MCP server + code indexing + REST backend client.
//!
//! This crate exposes:
//! - `config`: load/save configuration from `~/.augmcp/settings.toml`.
//! - `indexer`: incremental indexing with .gitignore and exclude patterns.
//! - `backend`: REST calls to upload blobs and perform retrieval.
//! - `server`: rmcp server with a `search_context` tool.

pub mod backend;
pub mod config;
pub mod indexer;
pub mod server;
pub mod service;
pub mod tasks;

pub use server::AugServer;
