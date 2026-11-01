//! Where the code index lives.
//!
//! The data folder is `<data dir>/harness-code/`, where the per-machine data folder is
//! `$XDG_DATA_HOME` (or `~/.local/share`) on Linux, `~/Library/Application Support` on macOS and
//! `%LOCALAPPDATA%` on Windows (not the roaming `%APPDATA%`: a SQLite database does not belong in a
//! roaming profile). [`DIR_VAR`] names another folder instead.
//!
//! The folder holds the database (`code.sqlite3`), the switch (`code.toml`) and the build status files
//! (`code-builds/`) Harness Desktop reads.
//!
//! Only the path is decided here: reading never creates the folder or the file.

use std::path::PathBuf;

use crate::support::{Env, OsKind};

/// Environment variable naming the data folder (empty = unset). Overrides the per-machine data
/// folder. A relative value is ignored (see [`override_dir`]).
pub const DIR_VAR: &str = "HARNESS_CODE_DIR";

/// The database file name.
pub const FILE_NAME: &str = "code.sqlite3";

/// The folder the server creates inside the per-machine data folder.
const APP_FOLDER: &str = "harness-code";

/// The per-machine data folder (see the module docs); `None` on Windows when `LOCALAPPDATA` is
/// unset. Relative values of `XDG_DATA_HOME` (ignored, as the XDG spec says) and `LOCALAPPDATA`
/// do not count: the server runs inside the project, so they would put the database there.
pub fn data_dir(env: &Env) -> Option<PathBuf> {
    let rooted = |name| env.var(name).map(PathBuf::from).filter(|dir| dir.has_root());

    match env.os {
        OsKind::Linux => Some(rooted("XDG_DATA_HOME").unwrap_or_else(|| env.home.join(".local/share"))),
        OsKind::MacOs => Some(env.home.join("Library/Application Support")),
        OsKind::Windows => rooted("LOCALAPPDATA"),
    }
}

/// The folder [`DIR_VAR`] names, with a leading `~` expanded. `None` when unset or relative: the
/// server runs with the project as its working directory, so a relative folder would put the
/// database inside the repository.
pub fn override_dir(env: &Env) -> Option<PathBuf> {
    env.var(DIR_VAR)
        .map(|dir| env.expand_home(dir))
        .filter(|dir| dir.has_root())
}

/// The folder that holds everything the code graph keeps: [`override_dir`] when set, else
/// `<data dir>/harness-code`. `None` when the data folder is unknown.
pub fn data_folder(env: &Env) -> Option<PathBuf> {
    if let Some(dir) = override_dir(env) {
        return Some(dir);
    }

    data_dir(env).map(|dir| dir.join(APP_FOLDER))
}

/// The database file: `<`[`data_folder`]`>/code.sqlite3`.
pub fn database_path(env: &Env) -> Option<PathBuf> {
    data_folder(env).map(|dir| dir.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_prefers_xdg_data_home() {
        let env = Env::for_tests("/home/u").with_os(OsKind::Linux);
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-code").join(FILE_NAME))
        );

        let env = env.with_var("XDG_DATA_HOME", "/data");
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/data/harness-code").join(FILE_NAME))
        );
    }

    #[test]
    fn macos_uses_application_support() {
        let env = Env::for_tests("/Users/u").with_os(OsKind::MacOs);

        assert_eq!(
            data_dir(&env),
            Some(PathBuf::from("/Users/u/Library/Application Support"))
        );
    }

    #[test]
    fn windows_uses_local_app_data() {
        let env = Env::for_tests("C:/Users/u").with_os(OsKind::Windows);
        assert_eq!(database_path(&env), None);

        // A folder that is absolute on the host running the test (the check uses the host's rules).
        let local = std::env::temp_dir().join("AppData").join("Local");
        let env = env.with_var("LOCALAPPDATA", local.to_string_lossy());
        assert_eq!(data_dir(&env), Some(local));
    }

    #[test]
    fn relative_data_folders_are_ignored() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var("XDG_DATA_HOME", "data");
        assert_eq!(data_dir(&env), Some(PathBuf::from("/home/u/.local/share")));

        let env = Env::for_tests("C:/Users/u")
            .with_os(OsKind::Windows)
            .with_var("LOCALAPPDATA", "AppData");
        assert_eq!(data_dir(&env), None);
    }

    #[test]
    fn a_relative_folder_override_is_ignored() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var(DIR_VAR, "data");

        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-code").join(FILE_NAME))
        );
    }

    #[test]
    fn the_folder_override_wins() {
        let env = Env::for_tests("/home/u")
            .with_os(OsKind::Linux)
            .with_var(DIR_VAR, "~/code");

        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u").join("code").join(FILE_NAME))
        );

        let env = env.with_var(DIR_VAR, "");
        assert_eq!(
            database_path(&env),
            Some(PathBuf::from("/home/u/.local/share/harness-code").join(FILE_NAME))
        );
    }
}
