# Plan: Harness Code (issue #48)

Move the code graph out of `harness-memory` into its own Harness family member, `harness-code`, so it works without
memory or knowledge and other members (Knowledge auto-links, #49) can reuse it.

## User decisions (2026-11-01)

- Own database under `<data>/harness-code/`. Works with neither memory nor knowledge installed.
- Nobody uses the memory code graph yet: no migration and no transition release. The next memory major removes `code_*`
  and deletes leftover code files from its data folder.
- Everything that works today keeps working, pointed at `harness-code`: the family hook "Use the code graph first", the
  Claude/Codex/OMP search hints, the OMP exploration nudge, build-on-open, the build toast, the on/off switch and the
  skills.
- CI on 3 OSes, semantic-release, checksums and installers like `harness-knowledge`. The desktop installs, registers and
  auto-updates `harness-code` like the other members.
- Logo: `harness-useful-logo/renders/code` (gradient `#DC2626` → `#F87171`), already done.

## Recommended choices taken (user was away, asked to execute the recommended path)

- Shared helpers are **copied** and trimmed (env, private files, paths, text, redact, git team vars, project resolver).
  No shared crate: the repos release independently.
- CLI drops the `code` prefix. The binary is the code graph.
- Version 1.0.0 for `harness-code`, 3.0.0 for `harness-memory`.
- The desktop gets a **Code** settings section holding the switch (moved from Settings → Memory). No rail view.
- The skills keep the `code-graph` stage name, so no settings migration.

## Contract of `harness-code`

| Item | Value |
| --- | --- |
| Binary | `harness-code` (`harness-code.exe`), installs to `~/.local/bin` / `%LOCALAPPDATA%\Programs\harness-code\` |
| Data folder | `<data>/harness-code/` (`$XDG_DATA_HOME` or `~/.local/share`, `~/Library/Application Support`, `%LOCALAPPDATA%`), override `HARNESS_CODE_DIR` |
| Files | `code.sqlite3` (schema as memory v2.11 user_version 18, restarted at 1), `code.toml` (`enabled = true\|false`), `code-builds/<16 hex>.json` |
| Env | `HARNESS_CODE_INDEX` (0\|1 switch override), `HARNESS_CODE_INPROCESS`, `HARNESS_CODE_BUDGET_MS`, `HARNESS_CODE_GRACE_MS` (captured), debug `HARNESS_CODE_BIN` |
| MCP | `harness-code` or `harness-code mcp`, server name `harness-code`, tools `code_map`, `code_symbol`, `code_impact`, `code_path` (all `_meta anthropic/alwaysLoad`) |
| CLI | `status\|enable\|disable [--json]`, `hint --project P --query TEXT [--json]`, `repo\|build --project P [--json]`, `match --project P [--name N]... [--path P]... --json`, `features`, `--version`, internal `code-worker` |
| features | `code-index`, `code-hint`, `code-prepare`, `code-match` |
| JSON | same shapes as memory's `code … --json` today. `match` returns `{"files":[{"path"}],"symbols":[{"name","kind","path","line"}]}` |

Switch semantics stay: missing file or key = on, unreadable or invalid = off, env wins.

As built (package C):
- `match` always carries `"state"` (`indexed`, `none`, `off`, `refused`, `tooLarge`, `failed`). It reads SQLite read-only
  without loading the graph, needs at least one `--name` or `--path` (at most 100 each) and lists at most 200 files and
  200 symbols. Names match exactly (`Type::name` too), paths exactly or as a suffix after `/`.
- Switch off: `tools/list` is empty and the off text points to Harness Desktop → Settings → Code.
- Extra env: `HARNESS_CODE_WORKER_MEMORY_MB`, test-only `HARNESS_CODE_WORKER_TEST`. The pid file `code-worker.pid` is
  debug only. Schema `user_version` is 18.
- `TEAM_VARS` also strips the memory and knowledge database variables from git and the worker.

## Work packages

### C — harness-useful-code (branch `feat/harness-code`)
Port `src/code/` from memory `main` with its tests, fixtures, snapshots, golden and bench. Scaffold from
harness-knowledge: Cargo.toml, toolchain, deny, build.rs version, CI (rust ×3 OS, deny, scripts), release pipeline,
`.releaserc.json`, `set-version.ts`, install.sh/ps1, README (with "Register with your harness"). Own MCP skeleton,
`match` read API, mimalloc kept for the worker arena.

### M — harness-useful-memory (branch `feat/remove-code-graph`)
Remove `src/code`, `cli_code.rs`, `calls_code.rs`, `tools_code.rs`, code env vars, `features` lines, code tests,
fixtures, snapshots, bench and code-only deps. On open, delete leftover `code.sqlite3*`, `code.toml`, `code-builds/`,
`code-worker.pid` from the data folder. Commit type `feat!` so semantic-release cuts 3.0.0. README updated.

### S — harness-useful-skills (branch `feat/harness-code`)
Every code-graph reference points to `harness-code`: doctor probe (`harness-code features` → `code-index`), `code-hint.js`
(`harness-code hint`), OMP nudge (`harness-code repo`, prefixes `mcp__harness_code_code_`), skills text naming the
server, README, plugin.json. Regenerate every bundle, all `--check`s and tests pass.

### D — harness-useful-desktop (branch `feat/harness-code`)
D1 backend: new member `code` in every closed enum (`EcosystemId`, `Companion`, `HookRequirement`, `HookCapabilities`,
`Members`, `RecommendedSetup`/`HarnessSetup` fields), catalog/releases/detect/installer/install URLs, MCP registration for
every harness, `Product` for code (relax the embedder bound), code features probe, move `memory_code*` to `code_*` over
the `harness-code` sidecar (data folder + env per contract), Tauri commands/capabilities, family hook member →
HarnessCode with Claude tool names `mcp__harness-code__code_*` (old text kept in `earlier`), fake agent + fixtures.
D2 UI: `FamilyProduct`/`FAMILY_NAMES`/gradient, Ecosystem item, onboarding companion + MCP column, family-updater
(auto-update), Code settings section with the moved switch, toasts unchanged, i18n in all locales, tests, mocks.

## Order

C, M, S and D1 in parallel (contract above is fixed). D2 after D1. Then verification, 3 review + 3 security passes,
local install of the binaries for the user's real test. Commits, pushes and releases wait for the user. Release order:
harness-code 1.0.0 → harness-memory 3.0.0 → skills → desktop.

## Acceptance

- `harness-code` installs, updates and registers from the desktop like the other members, for every harness.
- `code_*` tools behave as before (moved tests pass, golden unchanged).
- `harness-memory` 3.0 has no code module and deletes leftover code files.
- Hooks, hints, nudge, build-on-open, toast, switch and skills work against `harness-code`.

## Non-goals

Changing how the graph is built or ranked. Knowledge auto-linking (#49).

## Release notes

The code graph now lives in Harness Code, which is on by default. An earlier "off" choice made under Harness Memory does not carry over, so switch it off again in Settings under Code if you want it off. Harness Desktop installs Harness Code once on its own for a user who has Harness Memory without it.
