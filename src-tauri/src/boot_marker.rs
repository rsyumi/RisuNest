//! Whether the last start finished. The marker is a native file, so it survives a cleared
//! WebView store and records a start the renderer never lived to report, including a crash or a
//! forced stop. It is the only judgement of a failed start; the stage and suspect the renderer
//! writes are hints beside it.

use std::io;
use std::path::{Path, PathBuf};

/// What one start attempt recorded before it either finished or disappeared.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Attempt {
    pub(crate) started_at: i64,
    pub(crate) app_version: String,
    /// Starts that began and never completed, this one included.
    pub(crate) consecutive_failures: u32,
}

/// What the renderer branches on. Zero means the last start finished; one offers the choice, and
/// more than one opens the recovery shell without asking.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootDecision {
    pub(crate) consecutive_failures: u32,
    /// The attempt that never finished, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous: Option<Attempt>,
}

pub(crate) fn marker_path(app_data_root: &Path) -> PathBuf {
    app_data_root.join("boot").join("attempt.json")
}

fn read(app_data_root: &Path) -> io::Result<Option<Attempt>> {
    match std::fs::read(marker_path(app_data_root)) {
        // A marker the current build cannot read still proves a start that never finished, so it
        // counts as one failure rather than being ignored.
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).unwrap_or_default())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Records that a start has begun and reports what the last one left behind.
pub(crate) fn begin(
    app_data_root: &Path,
    app_version: &str,
    started_at: i64,
) -> io::Result<BootDecision> {
    let previous = read(app_data_root)?;
    let consecutive_failures = previous
        .as_ref()
        .map_or(0, |attempt| attempt.consecutive_failures.saturating_add(1));
    let path = marker_path(app_data_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("json.writing");
    std::fs::write(
        &staging,
        serde_json::to_vec(&Attempt {
            started_at,
            app_version: app_version.to_owned(),
            consecutive_failures,
        })
        .map_err(io::Error::other)?,
    )?;
    std::fs::rename(&staging, &path)?;
    Ok(BootDecision {
        consecutive_failures,
        previous,
    })
}

/// Records that the start finished. The next start sees no marker and begins from zero.
pub(crate) fn complete(app_data_root: &Path) -> io::Result<()> {
    match std::fs::remove_file(marker_path(app_data_root)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(offset: i64) -> i64 {
        1_700_000_000_000 + offset
    }

    #[test]
    fn a_start_that_finishes_leaves_the_next_one_counting_from_zero() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            begin(directory.path(), "1.0.0", now(0)).unwrap(),
            BootDecision {
                consecutive_failures: 0,
                previous: None,
            }
        );
        complete(directory.path()).unwrap();
        assert!(!marker_path(directory.path()).exists());
        assert_eq!(
            begin(directory.path(), "1.0.0", now(1))
                .unwrap()
                .consecutive_failures,
            0
        );
    }

    #[test]
    fn a_start_that_never_finishes_is_counted_and_summarised() {
        let directory = tempfile::tempdir().unwrap();
        begin(directory.path(), "1.0.0", now(0)).unwrap();

        let second = begin(directory.path(), "1.0.1", now(10)).unwrap();
        assert_eq!(second.consecutive_failures, 1);
        assert_eq!(
            second.previous,
            Some(Attempt {
                started_at: now(0),
                app_version: "1.0.0".to_owned(),
                consecutive_failures: 0,
            })
        );

        let third = begin(directory.path(), "1.0.1", now(20)).unwrap();
        assert_eq!(third.consecutive_failures, 2);
        assert_eq!(third.previous.unwrap().app_version, "1.0.1");
    }

    #[test]
    fn a_marker_this_build_cannot_read_still_counts_as_a_start_that_failed() {
        let directory = tempfile::tempdir().unwrap();
        let path = marker_path(directory.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a marker").unwrap();
        let decision = begin(directory.path(), "1.0.0", now(0)).unwrap();
        assert_eq!(decision.consecutive_failures, 1);
        assert_eq!(decision.previous, Some(Attempt::default()));
    }

    #[test]
    fn completing_a_start_that_left_no_marker_is_not_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        complete(directory.path()).unwrap();
    }
}
