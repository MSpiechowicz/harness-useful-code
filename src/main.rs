//! The `harness-code` executable: `harness-code [mcp]` serves MCP over stdio. `harness-code
//! status|enable|disable [--json]` reads and changes the switch, `harness-code hint --project PATH --query
//! TEXT [--json]` gives the one line a search hook shows, `harness-code repo|build --project PATH [--json]`
//! tell what the index holds for a project and build it, `harness-code match --project PATH [--name NAME]...
//! [--path PATH]... [--json]` lists the files and definitions the index holds for names and paths (all in
//! [`harness_code::commands`]), `harness-code features` lists what this build can do for Harness Desktop and
//! `harness-code --version` prints the version.

use std::process::ExitCode;

use harness_code::{cli, code, commands, mcp};

/// What this build can do, as `harness-code features` lists it: one token per line, for Harness Desktop to
/// tell what it can offer. A token is added in the same change that makes the feature work.
const FEATURES: [&str; 4] = ["code-index", "code-hint", "code-prepare", "code-match"];

/// mimalloc hands freed memory back to the operating system (glibc's allocator keeps most of it), and the
/// worker's address space reserve is set through it (`MIMALLOC_ARENA_RESERVE`, see `code::supervise`).
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<Option<&str>> = args.iter().map(|arg| arg.to_str()).collect();

    match args.as_slice() {
        [] | [Some("mcp")] => mcp::run(),
        [Some("--version")] => {
            println!("harness-code {}", env!("HARNESS_CODE_VERSION"));
            ExitCode::SUCCESS
        }
        [Some("features")] => {
            // One feature per line, for Harness Desktop to tell what it can offer.
            for feature in FEATURES {
                println!("{feature}");
            }

            ExitCode::SUCCESS
        }
        // started by the server only, never typed (see `harness_code::code::worker`)
        [Some(code::worker::COMMAND)] => code::worker::main(),
        [
            Some("status" | "enable" | "disable" | "hint" | "repo" | "build" | "match"),
            ..,
        ] => commands::command(&args),
        _ => cli::usage(commands::USAGE),
    }
}
