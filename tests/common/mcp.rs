//! An in-process MCP server, and git repositories to start it in.

use std::path::{Path, PathBuf};
use std::process::Command;

use harness_code::Server;
use harness_code::project::ProjectResolver;
use harness_code::support::Env;
use serde_json::{Value, json};

/// The path separator as a quoted reply field shows it under `~`: `/` on Unix, a JSON-escaped `\`
/// on Windows.
pub const SEP: &str = if cfg!(windows) { r"\\" } else { "/" };

/// `path` canonicalized the way the server keys and shows folders (no `\\?\` prefix on Windows).
pub fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).expect("canonical")
}

/// `path` as a quoted reply field shows it: a JSON string, so Windows backslashes are doubled.
pub fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("json")
}

/// An environment with `home` as the home folder and the real `PATH` (for git).
pub fn env(home: &Path) -> Env {
    Env::for_tests(home).with_var("PATH", std::env::var("PATH").unwrap_or_default())
}

/// A server whose working directory is `cwd` and whose home is `home`.
pub fn server(home: &Path, cwd: &Path) -> Server {
    Server::new(ProjectResolver::new(env(home), Some(cwd.to_path_buf())))
}

/// Answers `messages` one after the other, the way a client that waits for each answer sends them.
pub fn run(server: &mut Server, messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| server.handle(message.clone()))
        .collect()
}

pub fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

pub fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().expect("text content")
}

pub fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"].as_bool().expect("isError")
}

/// Runs `git args` in `dir`, ignoring the system git config.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A new repository `parent/name` (with a `src/deep` folder) whose `origin` is `remote`.
pub fn repo_with_remote(parent: &Path, name: &str, remote: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(dir.join("src/deep")).expect("mkdir");
    git(&dir, &["init", "-q"]);
    git(&dir, &["remote", "add", "origin", remote]);
    dir
}
