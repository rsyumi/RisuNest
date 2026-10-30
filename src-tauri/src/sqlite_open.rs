//! Every SQLite database the application opens is named through here, so all
//! connections to one file agree on the path SQLite sees.

use rusqlite::{Connection, OpenFlags, Result};
use std::path::Path;

pub(crate) fn open(path: impl AsRef<Path>) -> Result<Connection> {
    Connection::open(sqlite_path(path.as_ref()))
}

pub(crate) fn open_with_flags(path: impl AsRef<Path>, flags: OpenFlags) -> Result<Connection> {
    Connection::open_with_flags(sqlite_path(path.as_ref()), flags)
}

/// SQLite's Windows VFS treats a path starting with `\\` as a network share and
/// takes the shared-memory locks of every connection in the process through one
/// handle. Two connections reading at once there can leave a read lock behind
/// that no checkpoint passes, so a database is opened by its plain drive path
/// whenever that path names the same file.
#[cfg(windows)]
pub(crate) fn sqlite_path(path: &Path) -> &Path {
    let plain = path
        .to_str()
        .and_then(|path| path.strip_prefix(r"\\?\"))
        .filter(|rest| {
            let bytes = rest.as_bytes();
            // The log and shared-memory names add four characters to MAX_PATH.
            bytes.len() > 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1..3] == *b":\\"
                && rest.encode_utf16().count() + 4 < 260
        })
        .map(Path::new);
    match plain {
        Some(plain) if names_same_file(path, plain) => plain,
        _ => path,
    }
}

#[cfg(not(windows))]
pub(crate) fn sqlite_path(path: &Path) -> &Path {
    path
}

/// A database that does not exist yet is named by its directory, so the open
/// that creates it agrees with every later one.
#[cfg(windows)]
fn names_same_file(verbatim: &Path, plain: &Path) -> bool {
    use std::fs::canonicalize;
    let same = |left: &Path, right: &Path| {
        canonicalize(left).is_ok_and(|left| canonicalize(right).is_ok_and(|right| left == right))
    };
    // Win32 normalization has to leave the plain form as it is; otherwise it
    // names something else, such as a name with a trailing dot.
    std::path::absolute(plain).is_ok_and(|absolute| absolute == plain)
        && if verbatim.exists() {
            same(verbatim, plain)
        } else {
            matches!((verbatim.parent(), plain.parent()), (Some(left), Some(right)) if same(left, right))
        }
}

#[cfg(test)]
mod tests;
