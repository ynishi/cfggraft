//! Replacing a file without ever leaving a partial one behind.

use crate::error::{Error, Result};
use std::fs;
use std::path::Path;

/// Write `text` to `path` by way of a temporary file in the same directory.
///
/// The target belongs to a running program that may read it at any moment. A
/// truncate-then-write that dies halfway leaves that program holding half a
/// file; a rename never does, because a rename within a directory is atomic.
///
/// The temporary file is created alongside the target rather than in a system
/// temporary directory, since a rename across filesystems is not atomic and
/// would silently degrade to a copy.
///
/// An existing file's permissions are carried over: the rename would otherwise
/// hand the target whatever mode the temporary file was created with.
pub fn write(path: &Path, text: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::invalid(format!("{}: not a file path", path.display())))?;
    let tmp = dir.join(format!(".{}.cfggraft-tmp", name.to_string_lossy()));

    if let Err(e) = fs::create_dir_all(dir) {
        return Err(Error::io(dir, e));
    }
    fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    if let Ok(meta) = fs::metadata(path) {
        // Best effort: a filesystem that rejects the mode should not fail the
        // write, since the content is what matters.
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(path, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = env::temp_dir().join(format!("cfggraft-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn writes_and_replaces() {
        let p = scratch("a.txt");
        write(&p, "one").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        write(&p, "two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn creates_missing_directories() {
        let p = scratch("nested/deep/b.txt");
        write(&p, "x").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "x");
        let _ = fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn leaves_no_temporary_file_behind() {
        let p = scratch("c.txt");
        write(&p, "x").unwrap();
        let tmp = p.parent().unwrap().join(".c.txt.cfggraft-tmp");
        assert!(!tmp.exists());
        let _ = fs::remove_file(&p);
    }
}
