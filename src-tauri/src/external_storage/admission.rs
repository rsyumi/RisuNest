//! Reuses the existing admission; transport of finalized spools owns no PDS lock.
use crate::native_file_jobs::admission::{Admission, Permit};
use std::sync::Arc;
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Capture,
    PublishHead,
    Activate,
    Transport,
}
pub(crate) fn acquire(
    admission: &Arc<Admission>,
    phase: Phase,
) -> Result<Option<Permit>, &'static str> {
    match phase {
        Phase::Capture | Phase::Activate => admission.file(true).map(Some),
        Phase::PublishHead => admission.file(false).map(Some),
        Phase::Transport => Ok(None),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_and_head_follow_existing_replacement_admission() {
        let admission = Arc::new(Admission::default());
        let server = admission.server().unwrap();
        assert!(acquire(&admission, Phase::Capture).is_err());
        assert!(acquire(&admission, Phase::Transport).unwrap().is_none());
        drop(server);
        let publish = acquire(&admission, Phase::PublishHead).unwrap();
        assert!(acquire(&admission, Phase::Activate).is_err());
        drop(publish);
        assert!(acquire(&admission, Phase::Activate).is_ok());
    }
}
