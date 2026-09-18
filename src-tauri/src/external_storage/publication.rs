//! Small head publication protocol, independent of payload transfer. The owner
//! commits a PDS intent and obtains file(false) before the final recheck/write.
use super::{capabilities::Capabilities, contract::*};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadObservation {
    pub commit_id: String,
    pub authenticated_body_hash: String,
    pub version: Option<VersionToken>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationMode {
    Foreground,
    ExitDrain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublicationPermit {
    job_id: String,
    selection_epoch: String,
    mode: PublicationMode,
}
impl PublicationPermit {
    pub(super) fn new(
        job_id: String,
        selection_epoch: String,
        mode: PublicationMode,
    ) -> Result<Self> {
        if job_id.is_empty() || selection_epoch.is_empty() {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(Self {
            job_id,
            selection_epoch,
            mode,
        })
    }
    pub(crate) fn job_id(&self) -> &str {
        &self.job_id
    }
    pub(crate) fn selection_epoch(&self) -> &str {
        &self.selection_epoch
    }
    pub(crate) fn mode(&self) -> PublicationMode {
        self.mode
    }
}

#[cfg(test)]
pub(crate) fn test_publication_permit(
    job_id: &str,
    selection_epoch: &str,
    mode: PublicationMode,
) -> PublicationPermit {
    PublicationPermit::new(job_id.into(), selection_epoch.into(), mode).unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationObservation {
    Confirmed,
    Rejected,
    Unknown,
}

pub(crate) fn classify_publication(
    intended_commit: &str,
    intended_state: &str,
    authenticated_head: Option<(&str, &str)>,
    explicitly_rejected_same_request: bool,
) -> PublicationObservation {
    if authenticated_head == Some((intended_commit, intended_state)) {
        PublicationObservation::Confirmed
    } else if explicitly_rejected_same_request {
        PublicationObservation::Rejected
    } else {
        PublicationObservation::Unknown
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ready,
    Conflict,
    PublicationUnknown,
    Confirmed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublicationWrite {
    Cas(ExpectedHead),
    Sequential,
}
pub(crate) struct Attempt {
    strategy: PublicationStrategy,
    expected: Option<HeadObservation>,
    commit_id: String,
    body_hash: String,
    phase: Outcome,
    write_started: bool,
}
impl Attempt {
    pub fn new(
        capabilities: &Capabilities,
        strategy: PublicationStrategy,
        expected: Option<HeadObservation>,
        commit_id: String,
        body_hash: String,
    ) -> Result<Self> {
        capabilities.require(strategy)?;
        if commit_id.is_empty()
            || !crate::trust_boundary::is_lower_hex_256(&body_hash)
            || expected.as_ref().is_some_and(|h| {
                h.commit_id.is_empty()
                    || !crate::trust_boundary::is_lower_hex_256(&h.authenticated_body_hash)
            })
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        if strategy == PublicationStrategy::Cas
            && expected
                .as_ref()
                .is_some_and(|h| h.version.as_ref().is_none_or(|v| v.0.is_empty()))
        {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        Ok(Self {
            strategy,
            expected,
            commit_id,
            body_hash,
            phase: Outcome::Ready,
            write_started: false,
        })
    }
    pub fn before_write(
        &mut self,
        current: Option<&HeadObservation>,
        _mode: PublicationMode,
    ) -> Result<PublicationWrite> {
        if self.write_started || self.phase != Outcome::Ready {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let unchanged = match (self.expected.as_ref(), current) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.commit_id == b.commit_id
                    && a.authenticated_body_hash == b.authenticated_body_hash
                    && (self.strategy != PublicationStrategy::Cas || a.version == b.version)
            }
            _ => false,
        };
        if !unchanged {
            self.phase = Outcome::Conflict;
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        self.write_started = true;
        self.phase = Outcome::PublicationUnknown;
        Ok(match self.strategy {
            PublicationStrategy::Sequential => PublicationWrite::Sequential,
            PublicationStrategy::Cas => PublicationWrite::Cas(
                match self.expected.as_ref().and_then(|h| h.version.clone()) {
                    Some(v) => ExpectedHead::Exact(v),
                    None => ExpectedHead::Absent,
                },
            ),
        })
    }
    /// CAS precondition rejection is definite. Transport errors and ordinary
    /// writes stay unresolved until a fresh authenticated observation is available.
    pub fn write_failed(&mut self, error: &ProviderError) -> Outcome {
        if self.strategy == PublicationStrategy::Cas && error.kind == ErrorKind::PreconditionFailed
        {
            self.phase = Outcome::Conflict;
        }
        self.phase
    }
    pub fn observe_result(&mut self, current: Option<&HeadObservation>) -> Outcome {
        if self.write_started
            && self.phase == Outcome::PublicationUnknown
            && current.is_some_and(|head| {
                head.commit_id == self.commit_id && head.authenticated_body_hash == self.body_hash
            })
        {
            self.phase = Outcome::Confirmed;
        }
        self.phase
    }
}
