//! The private files and folders next to the database that Harness Desktop and the server share: the
//! switch file and the build status files of the code graph.
//!
//! On Unix the folders must be folders of this user that no one else may write to (`0700` when
//! the server makes them), the files are `0600`, written to a temporary file and renamed, and read
//! without following a symbolic link. Elsewhere the same calls skip the owner and mode checks.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::Env;

/// The folder that holds the database, where Harness Desktop and the server meet.
pub fn data_folder(env: &Env) -> Option<PathBuf> {
    crate::location::data_folder(env)
}

/// Creates the parent folder (0700 on Unix) and the file `path` (0600 on Unix) before SQLite opens it.
pub fn prepare_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);

        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

        builder.create(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    options.open(path).map(drop)
}

/// Creates `folder` (shown as `name` in errors) when missing and makes it `0700` on Unix
/// (failing for a folder another user owns).
pub fn private_folder(folder: &Path, name: &str) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

    builder
        .create(folder)
        .map_err(|error| format!("creating the {name} failed: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("securing the {name} failed: {error}"))?;
    }

    Ok(())
}

/// Writes `text` to `path` through a temporary file `<prefix>*.tmp` in `folder` (`0600` on Unix)
/// renamed into place, so a reader never sees half a file. `what` names the file in errors.
pub fn write_atomic(folder: &Path, path: &Path, text: &[u8], prefix: &str, what: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("writing the {what} failed: {error}");

    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tmp")
        .tempfile_in(folder)
        .map_err(failed)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }

    file.write_all(text).and_then(|()| file.flush()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;

    Ok(())
}

/// The bytes of the regular file `path`, at most `max` of them; `Ok(None)` when there is no such
/// file. Fails for a symbolic link or anything but a regular file, more than `max` bytes, and on
/// Unix a file of another user or one other users may write to.
pub fn read_private(path: &Path, max: usize) -> Result<Option<Vec<u8>>, String> {
    let shown = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{shown} cannot be read: {error}")),
    };

    if !metadata.file_type().is_file() {
        return Err(format!("{shown} is not a regular file"));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, nix::libc::O_NOFOLLOW);

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{shown} cannot be read: {error}")),
    };

    check_owner(&file, &shown)?;

    let mut bytes = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{shown} cannot be read: {error}"))?;

    if bytes.len() > max {
        return Err(format!("{shown} is larger than {max} bytes"));
    }

    Ok(Some(bytes))
}

/// Fails unless the opened `file` is a regular file of this user that no one else may write to.
#[cfg(unix)]
fn check_owner(file: &std::fs::File, shown: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|error| format!("{shown} cannot be read: {error}"))?;

    if !metadata.file_type().is_file() {
        return Err(format!("{shown} is not a regular file"));
    }

    if metadata.uid() != nix::unistd::getuid().as_raw() {
        return Err(format!("{shown} belongs to another user"));
    }

    if metadata.mode() & 0o022 != 0 {
        return Err(format!("other users may write to {shown}"));
    }

    Ok(())
}

#[cfg(not(unix))]
fn check_owner(_file: &std::fs::File, _shown: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn private_files_are_read_without_links_or_other_writers() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("a.json");
        assert_eq!(read_private(&path, 16), Ok(None));

        write_atomic(dir.path(), &path, b"{}", ".x-", "file").expect("written");
        assert_eq!(read_private(&path, 16), Ok(Some(b"{}".to_vec())));
        assert!(read_private(&path, 1).is_err());

        let link = dir.path().join("b.json");
        std::os::unix::fs::symlink(&path, &link).expect("link");
        assert!(read_private(&link, 16).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        assert!(read_private(&path, 16).is_err());
    }
}
