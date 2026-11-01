//! `harness-code match`: which files and definitions of a project the index holds for the names and the
//! paths another Harness family member asks about.
//!
//! Like [`super::hint`] it is read-only and costs next to nothing: it never builds, refreshes or creates the
//! index, never starts the worker and loads no graph. It opens `code.sqlite3` read-only and asks indexed
//! questions about the repository's current generation. A project without a usable index answers nothing
//! and says why ([`State`]).
//!
//! - A name matches the definitions that carry exactly that name (`name`), or `Type::name` and `Type.name`
//!   when the definition's parent or its path says `Type`.
//! - A path is repo-relative (a path under the project root is made so) and matches the file with that path,
//!   or whose path ends with it after a `/` (a suffix such as `lib.rs` or `src/lib.rs`).
//!
//! Each list is cut to [`MAX_RESULTS`], and the names and the paths read to [`MAX_ASKED`] each.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;

use super::prepare::{State, indexed};
use super::store::PACKAGE_LANG;
use crate::support::Env;
use crate::support::text::display_safe;

/// The most files, and the most definitions, an answer lists.
pub const MAX_RESULTS: usize = 200;

/// The most names, and the most paths, that are read from one question.
pub const MAX_ASKED: usize = 100;

/// The longest name or path that is asked about, in bytes.
const ASKED_LEN: usize = 512;

/// A file of the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileMatch {
    /// Repo-relative path, with `/`.
    pub path: String,
}

/// A definition of the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SymbolMatch {
    pub name: String,
    pub kind: String,
    /// Repo-relative path of the file that defines it.
    pub path: String,
    pub line: i64,
}

/// What the index holds for a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matches {
    /// [`State::Indexed`] when the lists come from the index, else why they are empty.
    pub state: State,
    pub files: Vec<FileMatch>,
    pub symbols: Vec<SymbolMatch>,
}

impl Matches {
    fn empty(state: State) -> Self {
        Self {
            state,
            files: Vec::new(),
            symbols: Vec::new(),
        }
    }
}

/// The files and definitions of the project at `project` that `paths` and `names` ask about.
pub fn matches(env: &Env, project: &str, names: &[&str], paths: &[&str]) -> Matches {
    let (conn, repo, root) = match indexed(env, project) {
        Ok(found) => found,
        Err(state) => return Matches::empty(state),
    };

    let paths: Vec<String> = paths
        .iter()
        .filter_map(|path| relative(path, &root))
        .take(MAX_ASKED)
        .collect();
    let names: Vec<&str> = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty() && name.len() <= ASKED_LEN)
        .take(MAX_ASKED)
        .collect();

    Matches {
        state: State::Indexed,
        files: files(&conn, repo, &paths).unwrap_or_default(),
        symbols: symbols(&conn, repo, &names).unwrap_or_default(),
    }
}

/// `path` as a repo-relative path with `/`: a path under `root` loses it, a leading `./` or `/` goes. `None`
/// for a blank or overlong path, or one that leaves the repository.
fn relative(path: &str, root: &Path) -> Option<String> {
    let path = path.trim();

    if path.is_empty() || path.len() > ASKED_LEN {
        return None;
    }

    let mut shown = path.replace('\\', "/");

    if let Some(root) = root.to_str() {
        let root = root.replace('\\', "/");

        if let Some(rest) = shown.strip_prefix(root.trim_end_matches('/'))
            && rest.starts_with('/')
        {
            shown = rest.to_owned();
        }
    }

    let shown = shown.trim_start_matches("./").trim_start_matches('/');

    if shown.is_empty() || shown.split('/').any(|part| part == "..") {
        return None;
    }

    Some(shown.to_owned())
}

/// The source files whose path is one of `paths` or ends with it after a `/`.
fn files(conn: &Connection, repo: i64, paths: &[String]) -> Option<Vec<FileMatch>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT path FROM files WHERE repo_id = ?1 AND lang <> ?2 AND hash <> '' AND \
             (path = ?3 OR (length(path) > length(?3) AND substr(path, -length(?3)) = ?3 \
                            AND substr(path, -length(?3) - 1, 1) = '/')) \
             ORDER BY path LIMIT ?4",
        )
        .ok()?;
    let mut found = BTreeSet::new();

    for path in paths {
        let rows = statement
            .query_map(rusqlite::params![repo, PACKAGE_LANG, path, MAX_RESULTS as i64], |row| {
                row.get::<_, String>(0)
            })
            .ok()?;

        // a repository may be hostile: a path with control or invisible characters is left out
        found.extend(
            rows.filter_map(Result::ok)
                .filter(|path| display_safe(path))
                .map(|path| FileMatch { path }),
        );
    }

    Some(found.into_iter().take(MAX_RESULTS).collect())
}

/// The definitions named `name` (`name`, `Type::name`, `Type.name`), ordered by path and line.
fn symbols(conn: &Connection, repo: i64, names: &[&str]) -> Option<Vec<SymbolMatch>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT s.name, s.kind, s.parent, f.path, s.line FROM symbols s JOIN files f ON f.id = s.file_id \
             WHERE s.repo_id = ?1 AND s.name = ?2 AND (?4 IS NULL OR s.parent = ?4 OR instr(f.path, ?4) > 0) \
             ORDER BY f.path, s.line LIMIT ?3",
        )
        .ok()?;
    let mut found = BTreeSet::new();

    for asked in names {
        // the last part is the name and the one before it its type (`std::fmt::Display`, `a.b.C.name`), filtered in
        // the query so a common name such as `new` cannot push `Type::new` past the limit
        let split = asked.rsplit_once("::").or_else(|| asked.rsplit_once('.'));
        let (qual, name) = match split {
            Some((qual, name)) => (Some(qual.rsplit(['.', ':']).next().unwrap_or(qual)), name),
            None => (None, *asked),
        };

        if name.is_empty() || qual.is_some_and(str::is_empty) {
            continue;
        }

        let rows = statement
            .query_map(rusqlite::params![repo, name, MAX_RESULTS as i64, qual], |row| {
                Ok(SymbolMatch {
                    name: row.get(0)?,
                    kind: row.get(1)?,
                    path: row.get(3)?,
                    line: row.get(4)?,
                })
            })
            .ok()?;

        found.extend(
            rows.filter_map(Result::ok)
                .filter(|symbol| display_safe(&symbol.name) && display_safe(&symbol.path)),
        );
    }

    // by path and line, as a reader would go through them
    let mut found: Vec<SymbolMatch> = found.into_iter().collect();
    found.sort_by(|left, right| (&left.path, left.line, &left.name).cmp(&(&right.path, right.line, &right.name)));
    found.truncate(MAX_RESULTS);

    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_made_repo_relative_and_never_leaves_the_repository() {
        let root = Path::new("/work/app");

        for (asked, expected) in [
            ("src/lib.rs", Some("src/lib.rs")),
            ("./src/lib.rs", Some("src/lib.rs")),
            ("/src/lib.rs", Some("src/lib.rs")),
            ("/work/app/src/lib.rs", Some("src/lib.rs")),
            ("src\\lib.rs", Some("src/lib.rs")),
            ("  lib.rs  ", Some("lib.rs")),
            ("/work/application/lib.rs", Some("work/application/lib.rs")),
            ("../lib.rs", None),
            ("src/../lib.rs", None),
            ("/work/app", Some("work/app")),
            ("", None),
            ("   ", None),
            ("/", None),
            ("./", None),
        ] {
            assert_eq!(relative(asked, root).as_deref(), expected, "{asked:?}");
        }

        assert_eq!(relative(&"x".repeat(ASKED_LEN + 1), root), None);
    }
}
