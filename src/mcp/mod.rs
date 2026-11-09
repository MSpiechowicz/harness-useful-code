//! The MCP server: newline-delimited JSON-RPC 2.0 over stdio, read synchronously one bounded line
//! at a time.
//!
//! - A line longer than [`LINE_MAX`] is answered with an error and skipped up to its newline.
//! - Unparsable JSON gets `-32700`, a batch (array) or a malformed request `-32600`, an unknown
//!   method `-32601`; serving continues after each.
//! - Notifications are never answered; answers from the client are ignored.
//! - Tool validation and index failures are tool results with `isError: true`.
//! - Nothing a client sends is written to stderr.
//! - The four code graph tools ([`calls`]) answer from the local code index. They are listed only while the
//!   switch ([`crate::code::switch`]) is on, and every call reads the switch again.
//! - A running index build stops when the client cancels its request, closes the input, or the server gets
//!   SIGTERM ([`input`]).

mod calls;
mod input;
pub mod protocol;
pub mod tools;

pub use input::stop_on_sigterm;

use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::code::CodeIndex;
use crate::limits::LINE_MAX;
use crate::project::ProjectResolver;
use crate::support::{Env, Stop};
use input::Watch;
use protocol::{INVALID_PARAMS, INVALID_REQUEST, Line, METHOD_NOT_FOUND, PARSE_ERROR};

/// How often the serving loop looks whether the server got SIGTERM while no input comes.
const TERMINATE_POLL: Duration = Duration::from_millis(100);

/// The state one server process keeps: the project resolver, the code index once a call opened it, and the
/// client's name.
pub struct Server {
    projects: ProjectResolver,
    client: Option<String>,
    /// Whether the code graph was on when the client initialized; the instructions and `tools/list` follow it
    /// (each `code_*` call reads the switch again).
    code_on: bool,
    /// The code index of this computer, opened by the first `code_*` call ([`calls`]).
    code: Option<CodeIndex>,
    /// Stops the build running now ([`input`]).
    stop: Stop,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server resolving repositories with `projects`.
    pub fn new(projects: ProjectResolver) -> Self {
        Self {
            projects,
            client: None,
            code_on: false,
            code: None,
            stop: Stop::default(),
        }
    }

    /// The server whose running build `stop` stops (see [`stop_on_sigterm`]); [`serve`] also fires it when
    /// the client cancels the request or closes the input.
    pub fn with_stop(mut self, stop: Stop) -> Self {
        self.stop = stop;
        self
    }

    /// Answers one parsed message; `None` for notifications and client answers.
    pub fn handle(&mut self, message: Value) -> Option<Value> {
        let null = Value::Null;

        let Some(object) = message.as_object() else {
            return Some(protocol::error(&null, INVALID_REQUEST, "Invalid Request"));
        };

        let id = object.get("id");
        let method = object.get("method").and_then(Value::as_str);
        let valid_id = id.is_none_or(|id| id.is_string() || id.is_number() || id.is_null());
        let reply_id = id.filter(|_| valid_id).cloned().unwrap_or(Value::Null);

        let Some(method) = method else {
            let is_answer = id.is_some() && (object.contains_key("result") || object.contains_key("error"));
            return (!is_answer).then(|| protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        };

        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id {
            return Some(protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        }

        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

        // A notification (no id) is never answered, whatever its method.
        let id = id?;

        let answer = match method {
            "initialize" => {
                self.client = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.code_on = calls::switched_on(self);

                protocol::result(id, protocol::initialize_result(&params, self.code_on))
            }
            "ping" => protocol::result(id, json!({})),
            "tools/list" => protocol::result(id, tools::definitions(self.code_on)),
            "tools/call" => self.tools_call(id, &params),
            _ => protocol::error(id, METHOD_NOT_FOUND, "Method not found"),
        };

        Some(answer)
    }

    fn tools_call(&mut self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return protocol::error(id, INVALID_PARAMS, "tools/call needs a tool name");
        };

        let mut arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        drop_empty_project_path(&mut arguments);

        match calls::call(self, name, arguments) {
            Some(reply) => protocol::result(id, tool_result(reply)),
            None => protocol::error(id, INVALID_PARAMS, &format!("unknown tool: {}", short(name))),
        }
    }
}

/// Removes a `project_path` that is blank text, `{}` or null: agents send those when they mean the repository
/// they are working in, and the call then resolves to the root of the folder the server runs in.
fn drop_empty_project_path(arguments: &mut Value) {
    let Some(fields) = arguments.as_object_mut() else {
        return;
    };

    let empty = match fields.get("project_path") {
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Object(object)) => object.is_empty(),
        Some(Value::Null) => true,
        _ => false,
    };

    if empty {
        fields.remove("project_path");
    }
}

/// The `tools/call` result for `reply`: its text and `isError`.
fn tool_result(reply: calls::Reply) -> Value {
    json!({
        "content": [{ "type": "text", "text": reply.text }],
        "isError": reply.is_error,
    })
}

/// Serves messages from `reader` until end of input or SIGTERM ([`stop_on_sigterm`]); fails only
/// when reading or writing fails. `reader` is read on a thread of its own ([`input`]).
pub fn serve<R: BufRead + Send + 'static, W: Write>(reader: R, mut writer: W, server: &mut Server) -> io::Result<()> {
    let watch = Watch::new(server.stop.clone());
    let (sender, lines) = mpsc::channel();
    input::spawn_reader(reader, watch.clone(), sender);

    loop {
        let line = match lines.recv_timeout(TERMINATE_POLL) {
            Ok(line) => line?,
            Err(RecvTimeoutError::Timeout) if server.stop.is_terminated() => return Ok(()),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };

        if server.stop.is_terminated() {
            return Ok(());
        }

        let reply = match line {
            Line::End => return Ok(()),
            Line::TooLong => Some(protocol::error(
                &Value::Null,
                INVALID_REQUEST,
                &format!("message exceeds {LINE_MAX} bytes"),
            )),
            Line::Text(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            Line::Text(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => answer(server, &watch, message),
                Err(_) => Some(protocol::error(&Value::Null, PARSE_ERROR, "Parse error")),
            },
        };

        if let Some(reply) = reply {
            protocol::write_message(&mut writer, &reply)?;
        }
    }
}

/// The answer to `message`, none when the client cancelled it while it ran.
fn answer(server: &mut Server, watch: &Watch, message: Value) -> Option<Value> {
    let request = message.get("method").and_then(|_| message.get("id")).cloned();

    watch.begin(request.as_ref());
    let reply = server.handle(message);
    let cancelled = watch.finish();

    reply.filter(|_| !cancelled)
}

/// `harness-code mcp`: serves MCP over stdio until the client closes its end or the server gets SIGTERM.
pub fn run() -> ExitCode {
    // Before anything starts a thread: SIGTERM then stops a running build before the server ends.
    let stop = Stop::default();
    stop_on_sigterm(&stop);

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-code: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let mut server = Server::new(ProjectResolver::new(env, std::env::current_dir().ok())).with_stop(stop);
    let stdin = BufReader::new(io::stdin());
    let mut stdout = io::stdout().lock();
    let served = serve(stdin, &mut stdout, &mut server);
    stdout.flush().ok();

    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness-code: stdio failed ({})", error.kind());
            ExitCode::FAILURE
        }
    }
}

/// At most 64 characters of agent text, for error messages.
fn short(text: &str) -> String {
    text.chars().take(64).collect()
}
