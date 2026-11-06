//! `tools/call`: the code graph tools `code_map`, `code_symbol`, `code_impact` and `code_path`.
//!
//! They answer from the local code index ([`crate::code`]) and nothing else. Every call reads the switch
//! again, resolves `project_path`, refreshes the index of that repository (which reports a first index or a
//! big change through the status file Harness Desktop reads, see [`crate::code::status`]) and replies with the
//! rendered text alone. Diagnostics go to stderr.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Server;
use super::tools::{
    IMPACT_DEPTH_DEFAULT, IMPACT_DEPTH_MAX, IMPACT_DEPTH_MIN, MAP_BUDGET_DEFAULT, MAP_BUDGET_MAX, MAP_BUDGET_MIN,
    PATH_DEPTH_DEFAULT, PATH_DEPTH_MAX, PATH_DEPTH_MIN, QUERY_CHARS_MAX, SYMBOL_LIMIT_DEFAULT, SYMBOL_LIMIT_MAX,
    SYMBOL_LIMIT_MIN,
};
use crate::code::status::StatusFile;
use crate::code::{CodeIndex, Graph, Refresh, open_index, switch};
use crate::project::code_root_refusal;
use crate::support::private::data_folder;
use crate::support::redact::redact_formats;

/// A tool result.
pub(super) struct Reply {
    pub text: String,
    pub is_error: bool,
}

/// Said while the switch is off.
const OFF: &str = "The code index is off. Turn it on in Harness Desktop → Settings → Code.";

/// The most bytes of a reply; the renderers stay far below it.
const REPLY_MAX: usize = 64 * 1024;

/// The first line of an answer served from what another session committed while it still builds.
const STALE: &str = "Note: another session is still building this index, so results may be incomplete.";

/// `reason`, a refusal of the arguments, with what the agent does next: an agent told only what is wrong tends to
/// give up on the tool and go on with grep.
fn call_again(reason: impl std::fmt::Display) -> String {
    format!("{reason}. Fix it and call the tool again.")
}

/// True when the code index is on now: the tools are listed only then.
pub(super) fn switched_on(server: &Server) -> bool {
    let env = server.projects.env();

    data_folder(env).is_some_and(|root| switch::enabled(env, &root))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapArgs {
    project_path: String,
    focus: Option<String>,
    query: Option<String>,
    budget: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolArgs {
    project_path: String,
    symbol: String,
    limit: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImpactArgs {
    project_path: String,
    symbol: String,
    depth: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    project_path: String,
    from: String,
    to: String,
    max_depth: Option<i64>,
}

/// Runs the tool `name`; `None` when there is no such tool.
pub(super) fn call(server: &mut Server, name: &str, arguments: Value) -> Option<Reply> {
    let result = match name {
        "code_map" => map(server, arguments),
        "code_symbol" => symbol(server, arguments),
        "code_impact" => impact(server, arguments),
        "code_path" => path(server, arguments),
        _ => return None,
    };

    Some(match result {
        Ok(text) => Reply { text, is_error: false },
        Err(text) => Reply { text, is_error: true },
    })
}

fn map(server: &mut Server, arguments: Value) -> Result<String, String> {
    check_on(server)?;

    let args: MapArgs = parse(arguments)?;
    let focus = args
        .focus
        .as_deref()
        .map(str::trim)
        .filter(|focus| !focus.is_empty())
        .map(|focus| short_enough("focus", focus))
        .transpose()?;
    let query = args
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(|query| short_enough("query", query))
        .transpose()?;
    let budget = bounded(args.budget, MAP_BUDGET_DEFAULT, MAP_BUDGET_MIN, MAP_BUDGET_MAX);

    answer(server, "code_map", &args.project_path, |graph| {
        graph.map(budget, focus, query)
    })
}

fn symbol(server: &mut Server, arguments: Value) -> Result<String, String> {
    check_on(server)?;

    let args: SymbolArgs = parse(arguments)?;
    let query = query_of(&args.symbol)?;
    let limit = bounded(args.limit, SYMBOL_LIMIT_DEFAULT, SYMBOL_LIMIT_MIN, SYMBOL_LIMIT_MAX);

    answer(server, "code_symbol", &args.project_path, |graph| {
        graph.symbol(query, limit)
    })
}

fn impact(server: &mut Server, arguments: Value) -> Result<String, String> {
    check_on(server)?;

    let args: ImpactArgs = parse(arguments)?;
    let query = query_of(&args.symbol)?;
    let depth = bounded(args.depth, IMPACT_DEPTH_DEFAULT, IMPACT_DEPTH_MIN, IMPACT_DEPTH_MAX);

    answer(server, "code_impact", &args.project_path, |graph| {
        graph.impact(query, depth)
    })
}

fn path(server: &mut Server, arguments: Value) -> Result<String, String> {
    check_on(server)?;

    let args: PathArgs = parse(arguments)?;
    let from = end_of(&args.from, "from")?;
    let to = end_of(&args.to, "to")?;
    let depth = bounded(args.max_depth, PATH_DEPTH_DEFAULT, PATH_DEPTH_MIN, PATH_DEPTH_MAX);

    answer(server, "code_path", &args.project_path, |graph| {
        graph.path(from, to, depth)
    })
}

/// The `from` or `to` the agent gave, trimmed and not empty.
fn end_of<'a>(value: &'a str, name: &str) -> Result<&'a str, String> {
    let value = value.trim();

    if value.is_empty() {
        return Err(call_again(format!("{name} must not be empty")));
    }

    short_enough(name, value)
}

/// Fails with [`OFF`] while the switch is off.
fn check_on(server: &Server) -> Result<(), String> {
    if switched_on(server) {
        Ok(())
    } else {
        Err(OFF.to_owned())
    }
}

/// `value` or its default, kept within `min..=max`.
fn bounded(value: Option<i64>, default: i64, min: i64, max: i64) -> usize {
    usize::try_from(value.unwrap_or(default).clamp(min, max)).unwrap_or(0)
}

/// The `symbol` the agent gave, trimmed and not empty.
fn query_of(symbol: &str) -> Result<&str, String> {
    let query = symbol.trim();

    if query.is_empty() {
        return Err(call_again("symbol must not be empty"));
    }

    short_enough("symbol", query)
}

fn short_enough<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    if value.chars().count() > QUERY_CHARS_MAX {
        return Err(call_again(format!(
            "{name} is longer than {QUERY_CHARS_MAX} characters"
        )));
    }

    Ok(value)
}

/// Refreshes the index of the repository `project_path` names and renders its graph with `render`.
fn answer(
    server: &mut Server,
    tool: &str,
    project_path: &str,
    render: impl FnOnce(&Graph) -> String,
) -> Result<String, String> {
    if project_path.trim().is_empty() {
        return Err(call_again(
            "project_path is required: pass the absolute path of the repository you are working in",
        ));
    }

    let root = server
        .projects
        .resolve(Some(project_path))
        .map_err(|reason| call_again(format!("{reason} (got {project_path:?})")))?
        .ok_or_else(|| {
            call_again(
                "project_path names the home folder or a filesystem root, which is too big to index. Pass the \
                 absolute path of the repository you are working in",
            )
        })?
        .root;

    // the code index reads and prints lines of the files it finds: only a git repository outside the hidden
    // folders of the home folder (settings, credentials) is read
    if let Some(refusal) = code_root_refusal(&server.projects.home(), &root) {
        return Err(refusal.message(&root));
    }

    let data = data_folder(server.projects.env()).ok_or("the data folder is unknown")?;
    let stop = server.stop.clone();
    let mut status = StatusFile::new(&data, &root);

    let index = index(server)?;
    let (graph, refresh) = index.graph_observed(&root, &|| stop.is_stopped(), &mut status)?;
    // a credential format or a URL with a password that a name or a signature still carries is masked (the
    // assignment rule is not applied: `token: String` is code, and the value of a constant is not kept)
    let mut text = redact_formats(&render(graph));

    report(tool, &refresh);

    // the graph is the last committed one, and the agent must not take it for the whole repository
    if refresh.stale {
        text = format!("{STALE}\n{text}");
    }

    cut(&mut text);

    Ok(text)
}

/// The code index, opened by the first call that needs it.
fn index(server: &mut Server) -> Result<&mut CodeIndex, String> {
    if server.code.is_none() {
        server.code = Some(open_index(server.projects.env())?);
    }

    server
        .code
        .as_mut()
        .ok_or_else(|| "the code index is not open".to_owned())
}

/// What the refresh did, for whoever reads the server's stderr.
fn report(tool: &str, refresh: &Refresh) {
    let graph = if refresh.graph_rebuilt {
        "graph loaded"
    } else {
        "graph cached"
    };
    let mut line = format!(
        "[harness-code] {tool}: {} files checked, {} read, {} removed, {graph}, {} ms",
        refresh.checked, refresh.reparsed, refresh.removed, refresh.ms
    );

    if let Some((id, kind)) = refresh.build {
        line.push_str(&format!(", build {id} ({})", kind.as_str()));
    }

    if refresh.skipped.total() > 0 {
        line.push_str(&format!(", {} files skipped", refresh.skipped.total()));
    }

    if refresh.stale {
        line.push_str(", answered from the last committed state while another session writes");
    }

    if refresh.skipped.truncated > 0 {
        line.push_str(&format!(", {} files cut at the size caps", refresh.skipped.truncated));
    }

    eprintln!("{line}");
}

/// Cuts `text` to [`REPLY_MAX`] bytes at a character boundary.
fn cut(text: &mut String) {
    if text.len() <= REPLY_MAX {
        return;
    }

    let mut end = REPLY_MAX;

    while !text.is_char_boundary(end) {
        end -= 1;
    }

    text.truncate(end);
    text.push_str("\n… (cut)");
}

/// The arguments of a call as `T`; the message tells the agent to check the tool's schema and call it again.
fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| {
        format!(
            "invalid arguments: {error}. Check the tool's input schema in tools/list for the required fields, then \
             call the tool again with them. project_path is the absolute git root of your repository."
        )
    })
}
