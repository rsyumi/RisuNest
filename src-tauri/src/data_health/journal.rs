//! What a repair changed, kept so it can be undone. The journal holds the previous image of
//! every record the repair rewrote and the hashes of the objects it stopped referencing, so the
//! unused file cleanup leaves those objects alone while the journal still names them.

use super::repair::RepairCandidate;
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// Journals kept before the oldest is dropped. A dropped journal releases the objects it held,
/// which return to the cleanup's candidates.
pub(crate) const KEPT: usize = 5;
/// Total bytes the journals may occupy. Whichever bound is reached first rotates.
pub(crate) const BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// One record the repair changed, as raw stored columns on both sides. `before` is absent for a
/// record the repair added and `after` for one it removed. Keeping both images is what lets an
/// undo tell a record the repair produced from one the reader edited afterwards.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordChange {
    pub(crate) table: String,
    pub(crate) identity: Vec<String>,
    pub(crate) before: Option<Vec<Option<String>>>,
    pub(crate) after: Option<Vec<Option<String>>>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Journal {
    pub(crate) id: String,
    pub(crate) created_at: i64,
    /// Revision the repair was applied to, and the revision it produced.
    pub(crate) from_revision: i64,
    pub(crate) to_revision: i64,
    pub(crate) applied: Vec<RepairCandidate>,
    pub(crate) records: Vec<RecordChange>,
    /// Objects that lost their last reference. Held against collection while this journal lives.
    pub(crate) released_objects: BTreeSet<String>,
}

pub(crate) fn directory(app_data_root: &Path) -> PathBuf {
    app_data_root
        .join("persistent")
        .join("data-health")
        .join("journals")
}

fn path(app_data_root: &Path, id: &str) -> PathBuf {
    directory(app_data_root).join(format!("{id}.json"))
}

/// Journals newest first. A file the current build cannot read is reported as absent rather
/// than failing the list, and rotation removes it like any other.
pub(crate) fn list(app_data_root: &Path) -> io::Result<Vec<Journal>> {
    let mut journals: Vec<Journal> = entries(app_data_root)?
        .into_iter()
        .filter_map(|(_, path)| std::fs::read(path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    journals.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(journals)
}

pub(crate) fn read(app_data_root: &Path, id: &str) -> io::Result<Option<Journal>> {
    match std::fs::read(path(app_data_root, id)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn write(app_data_root: &Path, journal: &Journal) -> io::Result<()> {
    let path = path(app_data_root, &journal.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("json.writing");
    std::fs::write(
        &staging,
        serde_json::to_vec(journal).map_err(io::Error::other)?,
    )?;
    std::fs::rename(&staging, &path)?;
    rotate(app_data_root)
}

pub(crate) fn remove(app_data_root: &Path, id: &str) -> io::Result<()> {
    match std::fs::remove_file(path(app_data_root, id)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Newest first, with each file's size and modification time.
fn entries(app_data_root: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let directory = directory(app_data_root);
    let listing = match std::fs::read_dir(&directory) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for entry in listing {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            // A file left behind by an interrupted write is not a journal.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or_default();
        entries.push((modified, metadata.len(), path));
    }
    entries.sort_by(|left, right| right.0.cmp(&left.0));
    Ok(entries
        .into_iter()
        .map(|(_, size, path)| (size, path))
        .collect())
}

/// Drops the oldest journals once either bound is reached. What a dropped journal held stops
/// being held, which is the point: the cleanup may reclaim it again.
pub(crate) fn rotate(app_data_root: &Path) -> io::Result<()> {
    let mut kept = 0;
    let mut bytes = 0;
    for (size, path) in entries(app_data_root)? {
        kept += 1;
        bytes += size;
        if kept > KEPT || bytes > BUDGET_BYTES {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Objects every live journal still holds, in the shape the cleanup's root sets use.
pub(crate) fn roots(
    app_data_root: &Path,
) -> io::Result<crate::asset_repository::migration_gc::AssetRootSet> {
    let mut roots = crate::asset_repository::migration_gc::AssetRootSet::default();
    for journal in list(app_data_root)? {
        roots.object_hashes.extend(journal.released_objects);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests;
