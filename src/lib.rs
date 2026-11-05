//! `harness-code`: a code graph MCP server for any coding agent harness.
//!
//! It indexes a repository's files with tree-sitter (Rust, TypeScript and JavaScript with their component
//! formats, Go, Python, SCSS and CSS, indented Sass, Less and HTML) and answers where code is defined, who uses it and how one thing reaches another.
//! Agents ask through four tools (`code_map`, `code_symbol`, `code_impact`, `code_path`), and Harness Desktop and
//! the other Harness family members through a few commands. It works without any other member installed.
//!
//! - [`code`]: the code index, the graph and the worker process that reads repositories within limits.
//! - [`commands`]: the `status`, `enable`, `disable`, `hint`, `repo`, `build` and `match` commands.
//! - [`cli`]: what the commands share: option parsing and the usage line.
//! - [`limits`]: per-call limits.
//! - [`location`]: where the data folder, the index and the switch live.
//! - [`mcp`]: the stdio JSON-RPC server and its tools.
//! - [`project`]: which repository a call is about, and which folders the index refuses to read.
//! - [`support`]: the environment, git runner, secret masking and text checks the rest uses.
//!
//! The data lives in the per-machine data folder ([`location::data_folder`]), never in a repository, and a read
//! never creates it.

pub mod cli;
pub mod code;
pub mod commands;
pub mod limits;
pub mod location;
pub mod mcp;
pub mod project;
pub mod support;

pub use mcp::{Server, serve};
