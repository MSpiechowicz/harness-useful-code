//! Per-call limits.

/// Longest input line (one JSON-RPC message), in bytes.
pub const LINE_MAX: usize = 1024 * 1024;
