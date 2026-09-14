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
        codes::RECORD_ORPHAN | codes::OBJECT_UNREFERENCED => Severity::Informational,
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
    pub(crate) occurrence: i64,
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

    pub(crate) fn at(mut self, source_path: impl Into<String>, occurrence: i64) -> Self {
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
    /// Returns false when the validator must stop at this finding instead of continuing.
    fn record(&mut self, finding: Finding) -> bool;
}

/// Keeps the first finding and stops the scan, reproducing the fail-fast contract.
#[derive(Default)]
pub(crate) struct FirstFinding(Option<Finding>);

impl FirstFinding {
    pub(crate) fn into_inner(self) -> Option<Finding> {
        self.0
    }
}

impl FindingSink for FirstFinding {
    fn record(&mut self, finding: Finding) -> bool {
        self.0 = Some(finding);
        false
    }
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
        if self.items.len() < self.limit {
            self.items.push(finding);
        } else {
            self.omitted += 1;
        }
        true
    }
}
