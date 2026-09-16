//! Shared diagnosis vocabulary. One rule set reports the same violation either as the single
//! error that stops a fail-fast caller or as one entry in the list a repair screen needs.

/// Codes are an open set. A violation nothing here classifies is still reported, as
/// [`codes::UNCLASSIFIED`], so an unknown failure shape never hides the rest of a scan.
pub(crate) mod codes {
    pub(crate) const REFERENCE_MISSING: &str = "reference-missing";
    pub(crate) const REFERENCE_INVALID: &str = "reference-invalid";
    pub(crate) const ALIAS_OBJECT_ABSENT: &str = "alias-object-absent";
    pub(crate) const ALIAS_OBJECT_MISMATCH: &str = "alias-object-mismatch";
    pub(crate) const RECORD_INVALID: &str = "record-invalid";
    pub(crate) const RECORD_ORPHAN: &str = "record-orphan";
    pub(crate) const AUTHORITY_INCOMPLETE: &str = "authority-incomplete";
    pub(crate) const OBJECT_UNREFERENCED: &str = "object-unreferenced";
    pub(crate) const UNCLASSIFIED: &str = "unclassified";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Severity {
    /// Blocks a backup, a snapshot or an activation.
    Blocking,
    /// The application starts, but this item is broken where it is used.
    Degraded,
    /// Nothing depends on the item; it is only a cleanup candidate.
    Informational,
}

fn severity_of(code: &str) -> Severity {
    match code {
        codes::REFERENCE_MISSING | codes::REFERENCE_INVALID => Severity::Degraded,
        codes::OBJECT_UNREFERENCED => Severity::Informational,
        _ => Severity::Blocking,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Owner {
    pub(crate) kind: String,
    pub(crate) id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Locator {
    pub(crate) source_path: String,
    pub(crate) occurrence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Target {
    pub(crate) kind: String,
    pub(crate) key: String,
}

/// `detail` carries the validator's own message. It never carries record content.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Finding {
    pub(crate) code: String,
    pub(crate) severity: Severity,
    pub(crate) owner: Owner,
    pub(crate) locator: Option<Locator>,
    pub(crate) target: Option<Target>,
    pub(crate) detail: String,
}

impl Finding {
    pub(crate) fn new(
        code: &'static str,
        owner_kind: impl Into<String>,
        owner_id: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            severity: severity_of(code),
            code: code.to_owned(),
            owner: Owner {
                kind: owner_kind.into(),
                id: owner_id.into(),
            },
            locator: None,
            target: None,
            detail: detail.into(),
        }
    }

    pub(crate) fn at(mut self, source_path: impl Into<String>, occurrence: u64) -> Self {
        self.locator = Some(Locator {
            source_path: source_path.into(),
            occurrence,
        });
        self
    }

    pub(crate) fn targeting(mut self, kind: impl Into<String>, key: impl Into<String>) -> Self {
        self.target = Some(Target {
            kind: kind.into(),
            key: key.into(),
        });
        self
    }
}

pub(crate) trait FindingSink {
    /// A rule violation. Returns false when the validator must stop instead of continuing.
    fn record(&mut self, finding: Finding) -> bool;
    /// An observation the activation contract accepts, such as a reference that has always been
    /// allowed to dangle. It belongs in a diagnosis but never fails a gate.
    fn note(&mut self, finding: Finding);
}

/// Keeps the first blocking finding and stops the scan, reproducing the fail-fast contract.
#[derive(Default)]
pub(crate) struct FirstFinding(Option<Finding>);

impl FirstFinding {
    pub(crate) fn into_inner(self) -> Option<Finding> {
        self.0
    }
}

impl FindingSink for FirstFinding {
    fn record(&mut self, finding: Finding) -> bool {
        if finding.severity != Severity::Blocking {
            return true;
        }
        self.0 = Some(finding);
        false
    }
    fn note(&mut self, _finding: Finding) {}
}

/// Keeps every finding up to a bound and counts the rest, so a thoroughly damaged library
/// cannot exhaust memory through its own diagnosis.
pub(crate) struct Findings {
    pub(crate) items: Vec<Finding>,
    pub(crate) omitted: u64,
    limit: usize,
}

impl Findings {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            items: Vec::new(),
            omitted: 0,
            limit,
        }
    }
}

impl FindingSink for Findings {
    fn record(&mut self, finding: Finding) -> bool {
        self.note(finding);
        true
    }
    fn note(&mut self, finding: Finding) {
        if self.items.len() < self.limit {
            self.items.push(finding);
        } else {
            self.omitted += 1;
        }
    }
}

/// Carries the stop decision across nested validators, so a fail-fast caller stops the whole
/// scan at its first blocking violation rather than only the loop that produced it.
pub(crate) struct Report<'a> {
    sink: &'a mut dyn FindingSink,
    stopped: bool,
}

impl<'a> Report<'a> {
    pub(crate) fn new(sink: &'a mut dyn FindingSink) -> Self {
        Self {
            sink,
            stopped: false,
        }
    }

    pub(crate) fn running(&self) -> bool {
        !self.stopped
    }
}

impl FindingSink for Report<'_> {
    fn record(&mut self, finding: Finding) -> bool {
        if self.stopped {
            return false;
        }
        self.stopped = !self.sink.record(finding);
        !self.stopped
    }
    fn note(&mut self, finding: Finding) {
        if !self.stopped {
            self.sink.note(finding);
        }
    }
}

pub(crate) mod journal;
pub(crate) mod repair;

/// The renderer asked for the stop, so its own loop ends quietly instead of reporting a failure.
pub(crate) const CANCELLED: &str = "data-health-scan-cancelled";

/// How much of the library a scan reads. The quick depth stays proportional to the database and
/// never opens a stored object; the deep depth rereads every registered object.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ScanDepth {
    Quick,
    Deep,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeverityCounts {
    pub(crate) blocking: u64,
    pub(crate) degraded: u64,
    pub(crate) informational: u64,
}

impl SeverityCounts {
    pub(crate) fn of(items: &[Finding]) -> Self {
        let mut counts = Self::default();
        for finding in items {
            match finding.severity {
                Severity::Blocking => counts.blocking += 1,
                Severity::Degraded => counts.degraded += 1,
                Severity::Informational => counts.informational += 1,
            }
        }
        counts
    }
}

/// Where a deep scan stopped. `cursor` is the last object hash it finished, so a resumed run
/// continues after it instead of rereading what it already hashed.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeepProgress {
    pub(crate) cursor: Option<String>,
    pub(crate) completed_objects: u64,
    pub(crate) total_objects: u64,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) complete: bool,
}

/// One diagnosis. It is persisted so a deep scan survives a restart and so the screen can show
/// the last result without scanning again.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScanResult {
    pub(crate) revision: i64,
    pub(crate) scanned_at: i64,
    pub(crate) depth: ScanDepth,
    pub(crate) counts: SeverityCounts,
    pub(crate) items: Vec<Finding>,
    pub(crate) omitted: u64,
    /// Present once a deep scan has started, complete or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deep: Option<DeepProgress>,
}

impl ScanResult {
    pub(crate) fn new(revision: i64, scanned_at: i64, depth: ScanDepth, findings: Findings) -> Self {
        Self {
            revision,
            scanned_at,
            depth,
            counts: SeverityCounts::of(&findings.items),
            items: findings.items,
            omitted: findings.omitted,
            deep: None,
        }
    }

    /// Merges a later phase into the result. The phase was bounded by the room this result had
    /// left, so the total still respects the scan limit.
    pub(crate) fn absorb(&mut self, findings: Findings) {
        self.items.extend(findings.items);
        self.omitted += findings.omitted;
        self.counts = SeverityCounts::of(&self.items);
    }

    pub(crate) fn remaining(&self, limit: usize) -> usize {
        limit.saturating_sub(self.items.len())
    }
}

/// The diagnosis result lives in the working folder beside the snapshots. It never enters the
/// live database, a backup or a sync projection, so a damaged library cannot travel as one.
pub(crate) fn result_path(app_data_root: &std::path::Path) -> std::path::PathBuf {
    app_data_root
        .join("persistent")
        .join("data-health")
        .join("result.json")
}

pub(crate) fn write_result(
    app_data_root: &std::path::Path,
    result: &ScanResult,
) -> std::io::Result<()> {
    let path = result_path(app_data_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("json.writing");
    std::fs::write(
        &staging,
        serde_json::to_vec(result).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&staging, &path)
}

/// A result file the current build cannot read is a stale artefact, not a failure to report.
pub(crate) fn read_result(app_data_root: &std::path::Path) -> std::io::Result<Option<ScanResult>> {
    match std::fs::read(result_path(app_data_root)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
