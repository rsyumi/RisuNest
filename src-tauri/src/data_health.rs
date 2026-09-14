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
    pub(crate) const COLD_UNDECODABLE: &str = "cold-undecodable";
    pub(crate) const AUTHORITY_INCOMPLETE: &str = "authority-incomplete";
    pub(crate) const OBJECT_UNREFERENCED: &str = "object-unreferenced";
    pub(crate) const UNCLASSIFIED: &str = "unclassified";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Owner {
    pub(crate) kind: String,
    pub(crate) id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Locator {
    pub(crate) source_path: String,
    pub(crate) occurrence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Target {
    pub(crate) kind: String,
    pub(crate) key: String,
}

/// `detail` carries the validator's own message. It never carries record content.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Finding {
    pub(crate) code: &'static str,
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
            code,
            severity: severity_of(code),
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
