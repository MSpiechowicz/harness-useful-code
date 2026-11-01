//! A tiny MCP client that drives the real `harness-code` executable over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use serde_json::{Value, json};

/// The executable under test.
pub const BINARY: &str = env!("CARGO_BIN_EXE_harness-code");

/// `harness-code mcp` started in `cwd` with its data folder in `data_dir` (`HARNESS_CODE_DIR`).
pub fn command(data_dir: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command.arg("mcp").current_dir(cwd).env("HARNESS_CODE_DIR", data_dir);
    command
}

/// Keeps `command` away from the developer's own settings: the switch variable is removed unless the test set
/// it on `command` itself.
pub fn without_ambient_settings(command: &mut Command) {
    let chosen = command
        .get_envs()
        .any(|(name, _)| name == std::ffi::OsStr::new("HARNESS_CODE_INDEX"));

    if !chosen {
        command.env_remove("HARNESS_CODE_INDEX");
    }
}

/// A running server after the `initialize` handshake.
pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// The `initialize` result.
    pub initialized: Value,
}

impl Client {
    /// Starts `command` with piped stdio and completes the handshake. The developer's own switch
    /// setting is not passed on (see [`without_ambient_settings`]).
    pub fn start(mut command: Command) -> Self {
        without_ambient_settings(&mut command);

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the server starts");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut client = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
            initialized: Value::Null,
        };

        client.initialized = client.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "clientInfo": { "name": "tests", "version": "1" } }),
        )["result"]
            .clone();
        client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        client
    }

    /// Sends a request and returns the whole reply.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));

        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read reply");
        assert!(read > 0, "the server closed stdout before answering {method}");

        let reply: Value = serde_json::from_str(&line).expect("each reply is one JSON line");
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// Calls tool `name` and returns its text, which must not be an error.
    pub fn call(&mut self, name: &str, arguments: Value) -> String {
        let reply = self.request("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().expect("text").to_owned();
        assert_eq!(reply["result"]["isError"], false, "{name}: {text}");
        text
    }

    /// Closes stdin and waits for the server to exit.
    pub fn finish(mut self) -> ExitStatus {
        drop(self.stdin.take());
        self.child.wait().expect("the server exits")
    }

    /// The server's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Reads the next answer.
    pub fn reply(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read reply");
        assert!(read > 0, "the server closed stdout");

        serde_json::from_str(&line).expect("each reply is one JSON line")
    }

    /// Closes stdin without waiting for the server.
    pub fn close_input(&mut self) {
        drop(self.stdin.take());
    }

    /// Waits up to `limit` for the server to exit; `None` when it still runs (it is then killed).
    pub fn exit_within(&mut self, limit: std::time::Duration) -> Option<ExitStatus> {
        let until = std::time::Instant::now() + limit;

        while std::time::Instant::now() < until {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        self.child.kill().ok();
        self.child.wait().ok();
        None
    }

    /// Sends one message.
    pub fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").expect("write request");
        stdin.flush().expect("flush request");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            self.child.kill().ok();
            self.child.wait().ok();
        }
    }
}
