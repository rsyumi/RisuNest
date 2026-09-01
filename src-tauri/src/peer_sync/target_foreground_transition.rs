use super::android_foreground::AndroidForegroundKey;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AndroidTargetForegroundTransitionPhase {
    Reserved,
    Running,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AndroidTargetForegroundTransitionError {
    Stale,
    NotReserved,
    NotRunning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidTargetForegroundTransition<Output> {
    pub(crate) foreground: AndroidForegroundKey,
    pub(crate) phase: AndroidTargetForegroundTransitionPhase,
    pub(crate) result: Option<Output>,
    pub(crate) error: Option<String>,
}

impl<Output> AndroidTargetForegroundTransition<Output> {
    pub(crate) fn reserved(foreground: AndroidForegroundKey) -> Self {
        Self {
            foreground,
            phase: AndroidTargetForegroundTransitionPhase::Reserved,
            result: None,
            error: None,
        }
    }

    pub(crate) fn matches(&self, foreground: &AndroidForegroundKey) -> bool {
        self.foreground == *foreground
    }

    pub(crate) fn is_running(&self) -> bool {
        self.phase == AndroidTargetForegroundTransitionPhase::Running
    }

    pub(crate) fn require_exact_reserved(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<(), AndroidTargetForegroundTransitionError> {
        if !self.matches(foreground) {
            return Err(AndroidTargetForegroundTransitionError::Stale);
        }
        if self.phase != AndroidTargetForegroundTransitionPhase::Reserved {
            return Err(AndroidTargetForegroundTransitionError::NotReserved);
        }
        Ok(())
    }

    pub(crate) fn mark_running(&mut self) {
        self.phase = AndroidTargetForegroundTransitionPhase::Running;
    }

    pub(crate) fn require_exact_running(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<(), AndroidTargetForegroundTransitionError> {
        if !self.matches(foreground) {
            return Err(AndroidTargetForegroundTransitionError::Stale);
        }
        if self.phase != AndroidTargetForegroundTransitionPhase::Running {
            return Err(AndroidTargetForegroundTransitionError::NotRunning);
        }
        Ok(())
    }

    pub(crate) fn publish_terminal(&mut self, outcome: Result<Output, String>) {
        self.phase = AndroidTargetForegroundTransitionPhase::Terminal;
        match outcome {
            Ok(result) => self.result = Some(result),
            Err(error) => self.error = Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_sync::android_foreground::{AndroidForegroundKey, AndroidForegroundLane};

    #[test]
    fn terminal_transition_requires_the_exact_reserved_then_running_identity() {
        let foreground = AndroidForegroundKey {
            lane: AndroidForegroundLane::P4Target,
            operation_id: "44444444-4444-4444-8444-444444444444".to_owned(),
            generation: 9,
        };
        let stale = AndroidForegroundKey {
            generation: 8,
            ..foreground.clone()
        };
        let mut transition =
            AndroidTargetForegroundTransition::<String>::reserved(foreground.clone());

        assert_eq!(
            transition.require_exact_reserved(&stale),
            Err(AndroidTargetForegroundTransitionError::Stale),
        );
        transition.require_exact_reserved(&foreground).unwrap();
        transition.mark_running();
        assert_eq!(
            transition.require_exact_reserved(&foreground),
            Err(AndroidTargetForegroundTransitionError::NotReserved),
        );
        transition.require_exact_running(&foreground).unwrap();
        transition.publish_terminal(Ok("done".to_owned()));

        assert_eq!(
            transition.phase,
            AndroidTargetForegroundTransitionPhase::Terminal
        );
        assert_eq!(transition.result.as_deref(), Some("done"));
        assert_eq!(transition.error, None);
    }

    #[test]
    fn identity_mismatch_wins_over_a_wrong_phase() {
        // Callers map Stale and the phase errors to distinct user-facing
        // messages and rely on this precedence for their exhaustive matches.
        let foreground = AndroidForegroundKey {
            lane: AndroidForegroundLane::P4Target,
            operation_id: "44444444-4444-4444-8444-444444444444".to_owned(),
            generation: 9,
        };
        let stale = AndroidForegroundKey {
            generation: 8,
            ..foreground.clone()
        };
        let mut transition =
            AndroidTargetForegroundTransition::<String>::reserved(foreground.clone());

        assert_eq!(
            transition.require_exact_running(&stale),
            Err(AndroidTargetForegroundTransitionError::Stale),
        );
        transition.mark_running();
        assert_eq!(
            transition.require_exact_running(&stale),
            Err(AndroidTargetForegroundTransitionError::Stale),
        );
        assert_eq!(
            transition.require_exact_reserved(&stale),
            Err(AndroidTargetForegroundTransitionError::Stale),
        );
    }
}
