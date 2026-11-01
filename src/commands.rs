//! The commands of the `harness-code` executable, where the person at the terminal (and Harness Desktop and
//! the other Harness family members on their behalf) read and change the code graph of this computer. There is
//! no MCP tool for them.
//!
//! - `harness-code status [--json]` prints `{"enabled": bool, "source": "file"|"default"|"env"}`: whether
//!   agents get the `code_*` tools, and what decided it (`code.toml`, nothing set so the default on, or the
//!   `HARNESS_CODE_INDEX` variable).
//! - `harness-code enable [--json]` and `harness-code disable [--json]` write `code.toml` and print the status
//!   as `status` does (so `source` is `env` while the variable overrides the file).
//!
//! - `harness-code hint --project PATH --query TEXT [--json]` prints the one line [`hint`] gives for a
//!   name (`{"hint":"..."}` or `{"hint":null}` with `--json`, the line or nothing without). It exits 0 whatever
//!   the answer, and never builds, refreshes or creates the index.
//!
//! - `harness-code repo --project PATH [--json]` prints what the index holds for a project. It reads
//!   the index and asks `git` for the repository root ([`repo_state`], never builds and never creates the index
//!   (`code.sqlite3`) or the build status folder): `{"state":"indexed"|"none"|"tooLarge"|"failed"|
//!   "refused"|"off","files":N,"symbols":N,"indexedAt":unixSeconds|null}`. It exits 0 for every state.
//! - `harness-code build --project PATH [--json]` builds the index of a project that has none, as a
//!   `code_*` call would (the status file Harness Desktop reads included, one build however many calls of the
//!   index it takes), and prints `{"built":true,"files":N}`. It ends within
//!   [`MAX_RUN`](crate::code::prepare::MAX_RUN) (617 seconds, plus rare short waits for a lock).
//!   A project in any other state is left alone: `{"built":false,"reason":"<state>"}`, so opening a project
//!   again, or one that is too large, does no work. An error prints `{"built":false,"reason":"error",
//!   "error":"..."}`. It exits 0 whatever happened.
//! - `harness-code match --project PATH [--name NAME]... [--path PATH]... [--json]` prints the files and
//!   definitions the index holds for the names and the paths ([`matches`]), read-only like `repo`:
//!   `{"state":"indexed"|...,"files":[{"path"}],"symbols":[{"name","kind","path","line"}]}`. The lists are
//!   empty while the project has no usable index, and `state` says why. It exits 0 for every state.
//!
//! Without `--json` the answer is one line of text. Failures exit 1 with the reason on stderr, usage
//! errors exit 2, as in [`crate::cli`].

use std::process::ExitCode;

use serde_json::json;

use crate::cli::{self, Args};
use crate::code::hint::hint;
use crate::code::matches::{Matches, matches};
use crate::code::prepare::{Built, Repo, State as RepoState, build, repo_state};
use crate::code::switch::{self, State};
use crate::support::Env;
use crate::support::private::data_folder;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "[mcp] | status|enable|disable [--json] | hint --project PATH --query TEXT [--json] | \
                         repo|build --project PATH [--json] | match --project PATH [--name NAME]... [--path PATH]... \
                         [--json] | features | --version";

/// What `code` was asked to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    Status,
    Set(bool),
}

/// `harness-code status|enable|disable <args>`, and the other commands by their name: `hint`, `repo`,
/// `build` and `match`.
pub fn command(args: &[Option<&str>]) -> ExitCode {
    if let [Some("hint"), rest @ ..] = args {
        return hinted(rest);
    }

    if let [Some("repo"), rest @ ..] = args {
        return repo(rest);
    }

    if let [Some("build"), rest @ ..] = args {
        return built(rest);
    }

    if let [Some("match"), rest @ ..] = args {
        return matched(rest);
    }

    let Some((ask, json)) = parse(args) else {
        return cli::usage(USAGE);
    };

    let Ok(env) = Env::from_process() else {
        eprintln!("harness-code: the home folder is unknown");
        return ExitCode::FAILURE;
    };

    let Some(root) = data_folder(&env) else {
        eprintln!("harness-code: the data folder is unknown");
        return ExitCode::FAILURE;
    };

    if let Ask::Set(on) = ask
        && let Err(message) = switch::set_enabled(&root, on)
    {
        eprintln!("harness-code: {message}");
        return ExitCode::FAILURE;
    }

    println!("{}", shown(switch::state(&env, &root), json));
    ExitCode::SUCCESS
}

/// `hint`: always exits 0, unless the arguments are wrong.
fn hinted(args: &[Option<&str>]) -> ExitCode {
    let Some((project, query, json)) = parse_hint(args) else {
        return cli::usage(USAGE);
    };
    let line = Env::from_process().ok().and_then(|env| hint(&env, project, query));

    if json {
        println!("{}", json!({ "hint": line }));
    } else if let Some(line) = line {
        println!("{line}");
    }

    ExitCode::SUCCESS
}

/// `repo`: always exits 0, unless the arguments are wrong.
fn repo(args: &[Option<&str>]) -> ExitCode {
    let Some((project, json)) = parse_project(args) else {
        return cli::usage(USAGE);
    };
    let found = match Env::from_process() {
        Ok(env) => repo_state(&env, project),
        Err(_) => Repo {
            state: RepoState::Off,
            files: 0,
            symbols: 0,
            indexed_at: None,
        },
    };

    println!("{}", shown_repo(&found, json));

    ExitCode::SUCCESS
}

/// `build`: always exits 0, unless the arguments are wrong.
fn built(args: &[Option<&str>]) -> ExitCode {
    let Some((project, json)) = parse_project(args) else {
        return cli::usage(USAGE);
    };
    let outcome = match Env::from_process() {
        Ok(env) => build(&env, project),
        Err(_) => Built::Error("the home folder is unknown".to_owned()),
    };

    println!("{}", shown_build(&outcome, json));

    ExitCode::SUCCESS
}

/// `match`: always exits 0, unless the arguments are wrong.
fn matched(args: &[Option<&str>]) -> ExitCode {
    let Some(asked) = parse_match(args) else {
        return cli::usage(USAGE);
    };
    let found = match Env::from_process() {
        Ok(env) => matches(&env, asked.project, &asked.names, &asked.paths),
        Err(_) => Matches {
            state: RepoState::Off,
            files: Vec::new(),
            symbols: Vec::new(),
        },
    };

    println!("{}", shown_matches(&found, asked.json));

    ExitCode::SUCCESS
}

/// What `match` was asked.
#[derive(Debug, PartialEq, Eq)]
struct Asked<'a> {
    project: &'a str,
    names: Vec<&'a str>,
    paths: Vec<&'a str>,
    json: bool,
}

/// `--project PATH [--name NAME]... [--path PATH]... [--json]`; `None` when the project is missing, nothing is
/// asked about, or anything else is given.
fn parse_match<'a>(args: &[Option<&'a str>]) -> Option<Asked<'a>> {
    let parsed = Args::parse_repeating(args, &["--json"], &["--project"], &["--name", "--path"])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    let names = parsed.values("--name");
    let paths = parsed.values("--path");

    if names.is_empty() && paths.is_empty() {
        return None;
    }

    Some(Asked {
        project: parsed.value("--project")?,
        names,
        paths,
        json: parsed.flag("--json"),
    })
}

/// `--project PATH [--json]`; `None` when the project is missing or anything else is given.
fn parse_project<'a>(args: &[Option<&'a str>]) -> Option<(&'a str, bool)> {
    let parsed = Args::parse(args, &["--json"], &["--project"])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    Some((parsed.value("--project")?, parsed.flag("--json")))
}

/// `--project PATH --query TEXT [--json]`; `None` when one is missing or anything else is given.
fn parse_hint<'a>(args: &[Option<&'a str>]) -> Option<(&'a str, &'a str, bool)> {
    let parsed = Args::parse(args, &["--json"], &["--project", "--query"])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    Some((
        parsed.value("--project")?,
        parsed.value("--query")?,
        parsed.flag("--json"),
    ))
}

/// The answer for `state`: the JSON object, or one line of text.
fn shown(state: State, json: bool) -> String {
    if json {
        return json!({ "enabled": state.enabled, "source": state.source.as_str() }).to_string();
    }

    let word = if state.enabled { "on" } else { "off" };

    format!("The code index is {word} ({}).", state.source.as_str())
}

/// The answer of `code repo`: the JSON object, or one line of text.
fn shown_repo(repo: &Repo, json: bool) -> String {
    if json {
        return json!({
            "state": repo.state.as_str(),
            "files": repo.files,
            "symbols": repo.symbols,
            "indexedAt": repo.indexed_at,
        })
        .to_string();
    }

    match repo.state {
        RepoState::Indexed => format!(
            "The code index holds {} and {} of this project.",
            counted(repo.files, "file", "files"),
            counted(repo.symbols, "symbol", "symbols")
        ),
        RepoState::None => "The code index has nothing of this project yet.".to_owned(),
        RepoState::TooLarge => "This project is too large for the code index.".to_owned(),
        RepoState::Failed => "The code index failed on this project.".to_owned(),
        RepoState::Refused => "The code index does not read this folder.".to_owned(),
        RepoState::Off => "The code index is off.".to_owned(),
    }
}

/// The answer of `match`: the JSON object, or one line of text.
fn shown_matches(found: &Matches, json: bool) -> String {
    if json {
        let files: Vec<_> = found.files.iter().map(|file| json!({ "path": file.path })).collect();
        let symbols: Vec<_> = found
            .symbols
            .iter()
            .map(|symbol| json!({ "name": symbol.name, "kind": symbol.kind, "path": symbol.path, "line": symbol.line }))
            .collect();

        return json!({ "state": found.state.as_str(), "files": files, "symbols": symbols }).to_string();
    }

    if found.state != RepoState::Indexed {
        return format!("The code index has no answer ({}).", found.state.as_str());
    }

    format!(
        "The code index holds {} and {} that match.",
        counted(found.files.len(), "file", "files"),
        counted(found.symbols.len(), "symbol", "symbols")
    )
}

/// `1 file` or `N files`.
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The answer of `code build`: the JSON object, or one line of text.
fn shown_build(built: &Built, json: bool) -> String {
    if json {
        return match built {
            Built::Done(files) => json!({ "built": true, "files": files }),
            Built::Skipped(state) => json!({ "built": false, "reason": state.as_str() }),
            Built::Error(error) => json!({ "built": false, "reason": "error", "error": error }),
        }
        .to_string();
    }

    match built {
        Built::Done(files) => format!(
            "Built the code index of this project ({}).",
            counted(*files, "file", "files")
        ),
        Built::Skipped(state) => format!("The code index was not built ({}).", state.as_str()),
        Built::Error(error) => format!("The code index could not be built: {error}"),
    }
}

/// `status`, `enable` or `disable`, and whether `--json` was given; `None` for anything else.
fn parse(args: &[Option<&str>]) -> Option<(Ask, bool)> {
    let (command, rest) = args.split_first()?;
    let parsed = Args::parse(rest, &["--json"], &[])?;

    if !parsed.positional.is_empty() {
        return None;
    }

    let ask = match (*command)? {
        "status" => Ask::Status,
        "enable" => Ask::Set(true),
        "disable" => Ask::Set(false),
        _ => return None,
    };

    Some((ask, parsed.flag("--json")))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::code::switch::Source;

    #[test]
    fn the_commands_and_the_json_flag_are_read() {
        assert!(parse(&[Some("status"), Some("--json")]) == Some((Ask::Status, true)));
        assert!(parse(&[Some("enable")]) == Some((Ask::Set(true), false)));
        assert!(parse(&[Some("disable"), Some("--json")]) == Some((Ask::Set(false), true)));
        assert!(parse(&[]).is_none());
        assert!(parse(&[Some("rebuild")]).is_none());
        assert!(parse(&[Some("status"), Some("extra")]).is_none());
        assert!(parse(&[Some("status"), Some("--yes")]).is_none());
    }

    #[test]
    fn the_hint_needs_a_project_and_a_query() {
        let ok = parse_hint(&[
            Some("--project"),
            Some("/w"),
            Some("--query"),
            Some("name"),
            Some("--json"),
        ]);

        assert_eq!(ok, Some(("/w", "name", true)));
        assert_eq!(
            parse_hint(&[Some("--query=name"), Some("--project=/w")]),
            Some(("/w", "name", false))
        );
        assert_eq!(parse_hint(&[Some("--project"), Some("/w")]), None);
        assert_eq!(parse_hint(&[Some("--query"), Some("name")]), None);
        assert_eq!(
            parse_hint(&[Some("--project"), Some("/w"), Some("--query"), Some("n"), Some("x")]),
            None
        );
        assert_eq!(
            parse_hint(&[Some("--project"), Some("/w"), Some("--query"), Some("n"), Some("--yes")]),
            None
        );
    }

    #[test]
    fn a_match_needs_a_project_and_something_to_ask_about() {
        let ok = parse_match(&[
            Some("--project"),
            Some("/w"),
            Some("--name"),
            Some("a"),
            Some("--path=src/b.rs"),
            Some("--name=c"),
            Some("--json"),
        ]);

        assert_eq!(
            ok,
            Some(Asked {
                project: "/w",
                names: vec!["a", "c"],
                paths: vec!["src/b.rs"],
                json: true,
            })
        );
        assert_eq!(parse_match(&[Some("--project"), Some("/w")]), None);
        assert_eq!(parse_match(&[Some("--name"), Some("a")]), None);
        assert_eq!(
            parse_match(&[Some("--project"), Some("/w"), Some("--name"), Some("a"), Some("x")]),
            None
        );
        assert_eq!(
            parse_match(&[
                Some("--project"),
                Some("/a"),
                Some("--project"),
                Some("/b"),
                Some("--name"),
                Some("a")
            ]),
            None
        );
        assert_eq!(
            parse_match(&[Some("--project"), Some("/w"), Some("--query"), Some("a")]),
            None
        );
    }

    #[test]
    fn a_match_is_one_json_object_or_one_line() {
        use crate::code::matches::{FileMatch, SymbolMatch};

        let found = Matches {
            state: RepoState::Indexed,
            files: vec![FileMatch {
                path: "src/a.rs".to_owned(),
            }],
            symbols: vec![SymbolMatch {
                name: "a".to_owned(),
                kind: "fn".to_owned(),
                path: "src/a.rs".to_owned(),
                line: 3,
            }],
        };

        assert_eq!(
            shown_matches(&found, true),
            r#"{"state":"indexed","files":[{"path":"src/a.rs"}],"symbols":[{"name":"a","kind":"fn","path":"src/a.rs","line":3}]}"#
        );
        assert_eq!(
            shown_matches(&found, false),
            "The code index holds 1 file and 1 symbol that match."
        );

        let none = Matches {
            state: RepoState::None,
            files: Vec::new(),
            symbols: Vec::new(),
        };

        assert_eq!(
            shown_matches(&none, true),
            r#"{"state":"none","files":[],"symbols":[]}"#
        );
        assert_eq!(shown_matches(&none, false), "The code index has no answer (none).");
    }

    #[test]
    fn the_status_is_one_json_object_or_one_line() {
        let state = State {
            enabled: true,
            source: Source::Default,
        };

        assert_eq!(shown(state, true), r#"{"enabled":true,"source":"default"}"#);
        assert_eq!(shown(state, false), "The code index is on (default).");
    }
}
