//! Helpers shared by the test crates: an in-process MCP server (`mcp`) and a client for the real
//! executable (`stdio`).
//!
//! Each test crate uses a different subset, hence the `dead_code` allowance.
#![allow(dead_code)]

pub mod mcp;
pub mod stdio;
