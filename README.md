<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/lockup-dark.svg">
    <img src="docs/brand/lockup-light.svg" alt="harness code" width="430">
  </picture>
</p>

# Harness Code

**A code graph for your AI coding agents, in one local binary.** `harness-code` is a stdio
[MCP](https://modelcontextprotocol.io) server that Claude Code, Codex, OMP, PI, OpenCode and any other
MCP client can start. It indexes a repository with tree-sitter and answers where code is defined, who
uses it, what breaks when it changes and how one thing reaches another, so an agent reads the answer
instead of grepping and opening files. It is the code side of the Harness family: [`harness-memory`](https://github.com/MSpiechowicz/harness-useful-memory) keeps
what agents learn, [`harness-knowledge`](https://github.com/MSpiechowicz/harness-useful-knowledge) keeps what is written down, and `harness-code` knows the code.
It works on its own, with neither of the others installed.

The index is a local SQLite file. Nothing is sent anywhere.

## Install

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/MSpiechowicz/harness-useful-code/main/install.sh | sh
```

This installs `~/.local/bin/harness-code`. Linux is x86_64 only, macOS is Apple Silicon or Intel.

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/MSpiechowicz/harness-useful-code/main/install.ps1 | iex"
```

This installs `%LOCALAPPDATA%\Programs\harness-code\harness-code.exe`. Both installers check the SHA-256
against the release's `checksums.txt` before installing anything. Run them again to update.

Harness Desktop installs it, registers the MCP server for every harness and keeps it up to date.

## Register with your harness

Harness Desktop does this for you. By hand, register `harness-code mcp` once per harness.

**Claude Code**

```sh
claude mcp add --scope user harness-code -- harness-code mcp
```

**Codex**

```sh
codex mcp add harness-code -- harness-code mcp
```

**OMP, PI, OpenCode and any other client that reads an `mcpServers` object** (OMP and PI in `mcp.json`,
OpenCode under `mcp` in `opencode.json` with `"type": "local"` and `"command": ["harness-code", "mcp"]`):

```json
{ "mcpServers": { "harness-code": { "command": "harness-code", "args": ["mcp"] } } }
```

Then ask an agent where something is defined. On Windows use the full path of `harness-code.exe` when the
harness does not see your user `PATH`.

## MCP tools

| Tool | What it does |
| --- | --- |
| `code_map` | A ranked map of a project's key files with their signatures, within a token budget (`budget` 200 to 4000, default 700), optionally centred on what the task names (`query`: a symbol, a file path or words such as `checkout retry`, read like a `code_path` end; the definitions it names are listed first even when no other file uses them, and one that names nothing is answered `nothing matches <query>; map without it:` before the plain map) and/or on a path prefix (`focus`, relative to the project, with or without `./` or the project root; one that matches no indexed file is answered `no indexed files under <focus>`). Tests are left out. Where no definition is used by another file, each file lists its top definitions instead. For an unfamiliar area, before grep |
| `code_symbol` | Where a function, type or constant is defined (`name`, `Type::name`, `module::name` or `path/fragment::name`; a repository-relative file path lists the file's most used definitions), what it uses and who uses it, with `path:line` and, for the first sites of each list, the line of source that makes the link. `limit` 1 to 50 (default 20). A link found only by a shared name is marked `(by name)`, an uncertain one `?` |
| `code_impact` | Everything that depends on a symbol or a file (all its definitions), by caller depth (`depth` 1 to 4, default 2), one `caller → callee` line each, tests summarised per file. For before a signature changes |
| `code_path` | How A reaches B (`from`, `to`, `max_depth` 1 to 10, default 8): the shortest chain of calls, renders, shared constants and name links, one hop per line with `path:line` and, under it, the call site. An end is a symbol, a repository-relative file path, a string that names no symbol (an event name, the definitions that write it) or plain words (`/useful-skills doctor`, `the diff view loads the lines`). Tries the other direction when nothing leads forward |

Every tool takes `project_path`, the absolute path of the repository, and is marked
`anthropic/alwaysLoad`, so Claude Code loads the four tools up front instead of deferring them. They are
listed only while the switch is on (see [Switch](#switch)). The server also sends instructions that tell
agents to use the tools before grep.

## How the index works

It is built from the project's files with tree-sitter (Rust, TypeScript and JavaScript, `.svelte`, `.vue`
and `.astro` files, Go, Python, SCSS and CSS). It is on by default.

- **Always fresh.** Each `code_*` call compares the size and modification time of the files with the
  index, reads only the ones that changed and answers from the updated graph. There is no watcher, no
  commit hook and nothing to rebuild.
- **Bounded.** The walk follows no symbolic links, stays on one file system, respects `.gitignore` and
  skips `node_modules`, `target`, `.venv`, `venv`, `__pycache__` and `vendor` anywhere, and `dist` and
  `build` at the top of the repository or next to a project manifest. Hidden folders that are not ignored
  are indexed (`.github`, `.storybook`), except the ones that hold tool state or caches (`.git`,
  `.svelte-kit`, `.next`, `.cache`, `.idea`, `.vscode`, ...). Files over 1 MB and files that are not UTF-8
  are skipped. A repository with more than 200,000 entries or 20,000 indexable files is refused
  (`repository too large to index`).
- **Only git repositories.** The tools read and print lines of the files they index, so `project_path`
  must resolve to a git work tree. Any other folder is refused with `The code index only works inside a
  git repository.` A hidden folder directly under the home folder (`~/.config`, `~/.ssh`), the home
  folder, the folders above it, `/` and system folders are refused too.
- **No values.** Every string literal in the first line of a definition is replaced by `"…"` in every
  signature, except a short literal in a type position of TypeScript, JavaScript and Python. A credential
  format (`sk_live_...`, `ghp_...`) or URL password that a name or signature still carries is masked in the
  reply. Paths, names and signatures are shown with line breaks, escapes and bidirectional controls
  written out, so a file name cannot forge a line of an answer.
- **Limited.** A repository is untrusted input, so it is indexed in a separate process
  (`harness-code code-worker`, started by the server and not meant to be run by hand) that has a memory
  limit and a time limit and is killed past them. A repository that breaks its worker is answered `This
  repository could not be indexed within the code index limits.` while the server keeps running. The
  memory limit is 2 GiB (`HARNESS_CODE_WORKER_MEMORY_MB`, 256 to 65536) on Linux, where the kernel
  enforces it. macOS does not enforce it reliably and Windows has no such limit, so there only the time
  limit and the crash isolation apply.
- **Weak links.** A link between files in different languages comes only from a string that names a
  function or component defined elsewhere. Replies mark it `(by name)`, and an uncertain edge `?`.

**Build status.** A first index of a repository that has source files, or a refresh that touches 200 files
or more at once (a branch switch), is a build. It writes `code-builds/<16 hex digits>.json` in the data
folder (mode 0600, replaced atomically), so Harness Desktop can show its progress:

```json
{"version": 1, "buildId": 42, "root": "/work/app", "repoName": "app", "kind": "full", "phase": "building",
 "filesTotal": 812, "filesDone": 120, "startedAt": 1761700000, "finishedAt": null, "error": null}
```

`kind` is `full` or `bulk`, `phase` is `building`, `ready` or `error`, and times are Unix seconds.

## Command line

```sh
harness-code status --json    # {"enabled": true, "source": "default"}
harness-code disable --json   # {"enabled": false, "source": "file"}, writes code.toml
harness-code enable --json    # {"enabled": true, "source": "file"}
harness-code hint --project . --query compute_total --json   # {"hint": "..."} or {"hint": null}
harness-code repo --project . --json    # {"state": "indexed", "files": 812, "symbols": 9120, "indexedAt": 1761700000}
harness-code build --project . --json   # {"built": true, "files": 812}
harness-code match --project . --name compute_total --path src/lib.rs --json
harness-code features         # code-index, code-hint, code-prepare, code-match
harness-code --version
```

Commands exit 0 on success, 1 on a failure and 2 for a usage error or an unknown command. Without `--json`
the answer is one line of text.

### Switch

`status` reports whether agents get the tools and what decided it: `file` (`code.toml` in the data folder,
`enabled = true|false`), `default` (no file, so on) or `env` (the `HARNESS_CODE_INDEX` variable, `0` or
`1`, which wins over the file). A `code.toml` that is another user's, writable by others or not valid
means off. Off hides the four tools from `tools/list` (a server reads the switch when the client
initializes), makes a call answer `The code index is off. Turn it on in Harness Desktop → Settings →
Code.`, and keeps the index file.

### Read-only answers for other tools

`hint`, `repo` and `match` never build, refresh or create the index and start no worker. They read
`code.sqlite3` read-only and answer only for a git repository the tools may read.

- `hint --project PATH --query NAME` is the one-line nudge a hook gives when an agent searches the text of
  a project for a name. It answers only when the index holds a definition of `NAME` (`name`, `Type::name`
  or `Type.name`, 3 to 80 characters). It exits 0 whatever the answer.
- `repo --project PATH` prints `{"state": "indexed"|"none"|"tooLarge"|"failed"|"refused"|"off", "files": N,
  "symbols": N, "indexedAt": unixSeconds|null}`.
- `build --project PATH` builds the index of a project that has none, like the first tool call would, and
  prints `{"built": true, "files": N}`. It is one build to Harness Desktop however many tries it takes (a
  repository too large for one call of two minutes is asked again with the time that is left of ten
  minutes), and it runs for about 617 seconds at most. A project in any other state is left alone
  (`{"built": false, "reason": "indexed"}`), so opening a project again costs nothing. An error prints
  `{"built": false, "reason": "error", "error": "..."}`.
- `match --project PATH [--name NAME]... [--path PATH]...` is for other Harness members that link their
  own data to code. It lists what the index holds for the names and paths (at least one is required, at
  most 100 of each are read, at most 200 of each are listed):

  ```json
  {"state": "indexed",
   "files": [{"path": "src/lib.rs"}],
   "symbols": [{"name": "compute_total", "kind": "fn", "path": "src/lib.rs", "line": 1}]}
  ```

  A name matches the definitions that carry exactly that name (`Type::name` and `Type.name` also need the
  type). A path is repo-relative (a path under the project root is made so) and matches the file with
  that path or one whose path ends with it after a `/`. While the project has no usable index the lists are
  empty and `state` says why (`none`, `tooLarge`, `failed`, `refused`, `off`).

## Data

Everything lives in the per-machine data folder, never in a repository: `$XDG_DATA_HOME/harness-code`
(`~/.local/share/harness-code`) on Linux, `~/Library/Application Support/harness-code` on macOS and
`%LOCALAPPDATA%\harness-code` on Windows.

| File | What it holds |
| --- | --- |
| `code.sqlite3` | The index of every repository on this computer (mode 0600) |
| `code.toml` | The switch: `enabled = true` or `false` |
| `code-builds/<16 hex>.json` | The status of a build, for Harness Desktop |

## Environment

| Variable | Meaning |
| --- | --- |
| `HARNESS_CODE_DIR` | Data folder to use instead of the default (absolute, `~` is expanded) |
| `HARNESS_CODE_INDEX` | `0` or `1`: overrides the switch file |
| `HARNESS_CODE_WORKER_MEMORY_MB` | Memory limit of the worker, 256 to 65536 (default 2048) |
| `HARNESS_CODE_INPROCESS`, `HARNESS_CODE_BUDGET_MS`, `HARNESS_CODE_GRACE_MS`, `HARNESS_CODE_BIN` | Test hooks, honoured by debug builds only |

## Build

```sh
cargo build --release
cargo test
```

`bench/` holds the benchmark and golden repositories of the index (`bench/fetch.sh`, `bench/ab/run.sh`).

## License

GPL-3.0-or-later, see `LICENSE`.
