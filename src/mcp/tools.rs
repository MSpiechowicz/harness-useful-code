//! The server instructions and the tool definitions (`tools/list`): `code_map`, `code_symbol`,
//! `code_impact` and `code_path`. They are listed only while the code graph is on.

use serde_json::{Value, json};

/// The map budget in tokens: default and bounds.
pub(super) const MAP_BUDGET_DEFAULT: i64 = 700;
pub(super) const MAP_BUDGET_MIN: i64 = 200;
pub(super) const MAP_BUDGET_MAX: i64 = 4000;

/// The `limit` of `code_symbol`: default and bounds.
pub(super) const SYMBOL_LIMIT_DEFAULT: i64 = 20;
pub(super) const SYMBOL_LIMIT_MIN: i64 = 1;
pub(super) const SYMBOL_LIMIT_MAX: i64 = 50;

/// The `depth` of `code_impact`: default and bounds.
pub(super) const IMPACT_DEPTH_DEFAULT: i64 = 2;
pub(super) const IMPACT_DEPTH_MIN: i64 = 1;
pub(super) const IMPACT_DEPTH_MAX: i64 = 4;

/// The `max_depth` of `code_path`: default and bounds.
pub(super) const PATH_DEPTH_DEFAULT: i64 = 8;
pub(super) const PATH_DEPTH_MIN: i64 = 1;
pub(super) const PATH_DEPTH_MAX: i64 = 10;

/// The longest `symbol`, `query`, `focus`, `from` or `to` accepted, in characters.
pub(super) const QUERY_CHARS_MAX: usize = 300;

/// Sent in the `initialize` result while the code graph is on.
pub const INSTRUCTIONS: &str = "\
Code graph of your repository, shared by every coding agent on this machine: where code is defined and who uses it.
- Before grep: code_map, code_symbol, code_impact, code_path. A to B: code_path (symbol, file, command or words). \
Grepped for a start? Use code_path from there.
- ALWAYS pass project_path (absolute git root of your repository).";

/// Sent in the `initialize` result while the code graph is off.
pub const INSTRUCTIONS_OFF: &str = "\
Code graph of your repository. It is turned off on this computer, so this server offers no tools.";

/// The server instructions: they point at the code tools while the code graph is on (`code_on`).
pub fn instructions(code_on: bool) -> String {
    if code_on { INSTRUCTIONS } else { INSTRUCTIONS_OFF }.to_owned()
}

/// Every tool, as `tools/list` returns them: the four code tools while the code graph is on (`code_on`), none
/// while it is off.
pub fn definitions(code_on: bool) -> Value {
    if !code_on {
        return json!({ "tools": [] });
    }

    let mut tools = vec![code_map(), code_symbol(), code_impact(), code_path()];

    tools.iter_mut().for_each(always_loaded);

    json!({ "tools": tools })
}

fn code_map() -> Value {
    tool(
        "code_map",
        "Ranked map of a project's key files and signatures. Use it first in an unfamiliar area. query centres it on \
         the task (symbol, file or words); focus on a path prefix.",
        json!({
            "project_path": project_path(),
            "query": text(
                "What the task is about: a symbol, a file path, or words (`checkout retry`). The map is centred on \
                 the definitions and files it names instead of on the whole repository",
            ),
            "focus": text("Path prefix to centre the map on, such as src/api"),
            "budget": ranged(
                MAP_BUDGET_DEFAULT,
                &format!("Size of the map in tokens, {MAP_BUDGET_MIN} to {MAP_BUDGET_MAX}"),
            ),
        }),
        &["project_path"],
    )
}

fn code_symbol() -> Value {
    tool(
        "code_symbol",
        "Where a function, type or constant (or a file's definitions) is defined, what it uses and who uses it, with \
         file:line and the call-site source line. Prefer it to grep. `(by name)` and `?` mark weak links.",
        json!({
            "project_path": project_path(),
            "symbol": text("name, Type::name, module::name, path/fragment::name or a file path"),
            "limit": ranged(
                SYMBOL_LIMIT_DEFAULT,
                &format!("Entries per list, {SYMBOL_LIMIT_MIN} to {SYMBOL_LIMIT_MAX}"),
            ),
        }),
        &["project_path", "symbol"],
    )
}

fn code_impact() -> Value {
    tool(
        "code_impact",
        "Everything that depends on a symbol or a file, by caller depth, as caller → callee. Use it before changing a \
         signature or behaviour. `(by name)` and `?` mark weak links.",
        json!({
            "project_path": project_path(),
            "symbol": text("name, Type::name, module::name, path/fragment::name or a file path"),
            "depth": ranged(
                IMPACT_DEPTH_DEFAULT,
                &format!("Caller hops, {IMPACT_DEPTH_MIN} to {IMPACT_DEPTH_MAX}"),
            ),
        }),
        &["project_path", "symbol"],
    )
}

fn code_path() -> Value {
    tool(
        "code_path",
        "How A reaches B: the shortest chain of calls, renders, shared constants and name links, with call-site \
         file:line and source. Ends: symbol, file, command or plain words. If you grepped for a start, call this \
         from there.",
        json!({
            "project_path": project_path(),
            "from": text(
                "Where the chain starts: a symbol (name, Type::name, path/fragment::name), a file path, a command \
                 or event name, or plain words of the question"
            ),
            "to": text("Where the chain ends, written the same way"),
            "max_depth": ranged(
                PATH_DEPTH_DEFAULT,
                &format!("Longest chain in hops, {PATH_DEPTH_MIN} to {PATH_DEPTH_MAX}"),
            ),
        }),
        &["project_path", "from", "to"],
    )
}

/// A number the server keeps within its range (a value outside it is cut to the nearest end). The range is
/// said in the description, not as `minimum`/`maximum`: a client that validates the schema would refuse the
/// call instead (`budget: must be <= 4000`), and the agent would have to call again.
fn ranged(default: i64, description: &str) -> Value {
    json!({
        "type": "integer",
        "default": default,
        "description": format!("{description} (a value outside is cut to the nearest end)"),
    })
}

fn project_path() -> Value {
    text("Absolute path of the repository you are working in (its git root)")
}

/// Marks `tool` to be loaded up front. Claude Code defers MCP tools (the model sees only their names and loads a
/// schema with ToolSearch before a call), and an agent that is told to use a deferred tool first often goes on
/// with the tools it already has. A client that knows the mark loads the tool with the others; any other client
/// ignores `_meta`.
fn always_loaded(tool: &mut Value) {
    if let Some(object) = tool.as_object_mut() {
        object.insert("_meta".to_owned(), json!({ "anthropic/alwaysLoad": true }));
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

fn text(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}
