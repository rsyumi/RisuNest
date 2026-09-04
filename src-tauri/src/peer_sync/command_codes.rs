//! The single place a peer sync failure becomes a code the interface can map to
//! its own wording. Native detail stays in the native log; only these codes
//! cross a command boundary.
use super::PeerSyncError;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PeerCommandCode {
    SourceMissing,
    AuthorizationExpired,
    PermissionDenied,
    LaneUnavailable,
    IdentityMismatch,
    TransportUnavailable,
    RegistrationBlockedByActiveWork,
    SourceInUse,
    SourceChanged,
    DeltaCompletionRetained,
    PeerOutdated,
    OperationFailed,
}

const ALL_CODES: [PeerCommandCode; 12] = [
    PeerCommandCode::SourceMissing,
    PeerCommandCode::AuthorizationExpired,
    PeerCommandCode::PermissionDenied,
    PeerCommandCode::LaneUnavailable,
    PeerCommandCode::IdentityMismatch,
    PeerCommandCode::TransportUnavailable,
    PeerCommandCode::RegistrationBlockedByActiveWork,
    PeerCommandCode::SourceInUse,
    PeerCommandCode::SourceChanged,
    PeerCommandCode::DeltaCompletionRetained,
    PeerCommandCode::PeerOutdated,
    PeerCommandCode::OperationFailed,
];

impl PeerCommandCode {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::SourceMissing => "sourceMissing",
            Self::AuthorizationExpired => "authorizationExpired",
            Self::PermissionDenied => "permissionDenied",
            Self::LaneUnavailable => "laneUnavailable",
            Self::IdentityMismatch => "identityMismatch",
            Self::TransportUnavailable => "transportUnavailable",
            Self::RegistrationBlockedByActiveWork => "registrationBlockedByActiveWork",
            Self::SourceInUse => "sourceInUse",
            Self::SourceChanged => "sourceChanged",
            Self::DeltaCompletionRetained => "deltaCompletionRetained",
            Self::PeerOutdated => "peerOutdated",
            Self::OperationFailed => "operationFailed",
        }
    }
}

impl fmt::Display for PeerCommandCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// True when a string a lane already produced is one of the bounded codes, so a
/// re-bounding step passes it through instead of collapsing it to
/// `operationFailed`.
pub(crate) fn is_bounded_code(value: &str) -> bool {
    ALL_CODES.iter().any(|code| code.code() == value)
}

/// Validation payloads are matched whole. A message that only starts with or
/// contains a stable constant is a different failure and stays generic.
pub(crate) fn code_for(error: &PeerSyncError) -> PeerCommandCode {
    match error {
        PeerSyncError::Validation(payload) => validation_code(payload.as_str()),
        PeerSyncError::Transport(_) => PeerCommandCode::TransportUnavailable,
        _ => PeerCommandCode::OperationFailed,
    }
}

fn validation_code(payload: &str) -> PeerCommandCode {
    match payload {
        super::registry_commands::REGISTRATION_BLOCKED_BY_ACTIVE_WORK => {
            PeerCommandCode::RegistrationBlockedByActiveWork
        }
        super::registry_commands::SOURCE_IN_USE => PeerCommandCode::SourceInUse,
        super::registry_commands::REGISTERED_SOURCE_CHANGED => PeerCommandCode::SourceChanged,
        super::delta_completion::DELTA_COMPLETION_AMBIGUOUS => {
            PeerCommandCode::DeltaCompletionRetained
        }
        super::lan::PEER_OUTDATED => PeerCommandCode::PeerOutdated,
        _ => PeerCommandCode::OperationFailed,
    }
}

/// Leaves the failure detail in the native log and returns only the code.
pub(crate) fn finish_peer_command<T>(
    context: &str,
    result: Result<T, PeerSyncError>,
) -> Result<T, String> {
    result.map_err(|error| {
        crate::nlog!("warn", "{context} failed: {error}");
        code_for(&error).code().to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_sync::{
        delta_completion::DELTA_COMPLETION_AMBIGUOUS,
        lan::PEER_OUTDATED,
        registry_commands::{
            REGISTERED_SOURCE_CHANGED, REGISTRATION_BLOCKED_BY_ACTIVE_WORK, SOURCE_IN_USE,
        },
    };

    fn latest_command_code_log_containing(marker: &str) -> crate::native_log::LogEntry {
        crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .rev()
            .find(|entry| {
                entry.target.ends_with("peer_sync::command_codes") && entry.message.contains(marker)
            })
            .expect("command code failure log")
    }

    #[test]
    fn every_code_keeps_the_string_the_interface_classifies() {
        assert_eq!(
            ALL_CODES.map(PeerCommandCode::code),
            [
                "sourceMissing",
                "authorizationExpired",
                "permissionDenied",
                "laneUnavailable",
                "identityMismatch",
                "transportUnavailable",
                "registrationBlockedByActiveWork",
                "sourceInUse",
                "sourceChanged",
                "deltaCompletionRetained",
                "peerOutdated",
                "operationFailed",
            ]
        );
        assert!(ALL_CODES
            .iter()
            .all(|code| is_bounded_code(code.code()) && code.to_string() == code.code()));
        assert!(!is_bounded_code("Validation(\"peer-source-in-use\")"));
    }

    #[test]
    fn every_stable_validation_constant_maps_to_its_own_code() {
        for (payload, expected) in [
            (
                REGISTRATION_BLOCKED_BY_ACTIVE_WORK,
                PeerCommandCode::RegistrationBlockedByActiveWork,
            ),
            (SOURCE_IN_USE, PeerCommandCode::SourceInUse),
            (REGISTERED_SOURCE_CHANGED, PeerCommandCode::SourceChanged),
            (
                DELTA_COMPLETION_AMBIGUOUS,
                PeerCommandCode::DeltaCompletionRetained,
            ),
            (PEER_OUTDATED, PeerCommandCode::PeerOutdated),
        ] {
            assert_eq!(
                code_for(&PeerSyncError::Validation(payload.to_owned())),
                expected
            );
        }
    }

    #[test]
    fn a_payload_that_only_resembles_a_constant_stays_generic() {
        for payload in [
            format!("{SOURCE_IN_USE}-extra"),
            format!("refused: {SOURCE_IN_USE}"),
            SOURCE_IN_USE[..SOURCE_IN_USE.len() - 1].to_owned(),
            format!("Validation(\"{REGISTRATION_BLOCKED_BY_ACTIVE_WORK}\")"),
            PEER_OUTDATED.to_uppercase(),
            "registered delta source is used by an active target".to_owned(),
        ] {
            assert_eq!(
                code_for(&PeerSyncError::Validation(payload.clone())),
                PeerCommandCode::OperationFailed,
                "{payload} must not be classified as a refusal"
            );
        }
    }

    #[test]
    fn transport_is_the_only_variant_with_a_code_of_its_own() {
        assert_eq!(
            code_for(&PeerSyncError::Transport("socket detail".to_owned())),
            PeerCommandCode::TransportUnavailable
        );
        for error in [
            PeerSyncError::Cancelled,
            PeerSyncError::AlreadyActivated,
            PeerSyncError::ChunkHashMismatch {
                object: "object".to_owned(),
                offset: 3,
            },
            PeerSyncError::WholeObjectHashMismatch {
                object: "object".to_owned(),
            },
            PeerSyncError::StaleManifest {
                expected: "a".to_owned(),
                received: "b".to_owned(),
            },
            PeerSyncError::ActivationConflict {
                expected: None,
                actual: None,
            },
            PeerSyncError::LogicalMergeConflict {
                record: "record".to_owned(),
            },
            PeerSyncError::Protocol("protocol detail".to_owned()),
            PeerSyncError::Storage("storage detail".to_owned()),
            PeerSyncError::Validation("unrecognized detail".to_owned()),
        ] {
            assert_eq!(code_for(&error), PeerCommandCode::OperationFailed);
        }
    }

    #[test]
    fn a_finished_command_returns_the_code_and_logs_the_detail() {
        let error = finish_peer_command::<()>(
            "command-codes-fixture-detail clone target",
            Err(PeerSyncError::Storage(
                "local target state at C:/private/path is unavailable".to_owned(),
            )),
        )
        .unwrap_err();

        assert_eq!(error, "operationFailed");
        let entry = latest_command_code_log_containing("command-codes-fixture-detail");
        assert!(entry.message.contains("clone target"));
        assert!(entry.message.contains("C:/private/path"));
    }

    #[test]
    fn a_finished_command_masks_secrets_in_the_native_log_it_keeps() {
        let error = finish_peer_command::<()>(
            "command-codes-fixture-masking transport",
            Err(PeerSyncError::Transport(
                "Authorization: Bearer fixture-command-secret".to_owned(),
            )),
        )
        .unwrap_err();

        assert_eq!(error, "transportUnavailable");
        let entry = latest_command_code_log_containing("command-codes-fixture-masking");
        assert!(entry.message.contains("Authorization: ***"));
        assert!(!entry.message.contains("fixture-command-secret"));
    }

    #[test]
    fn a_successful_command_is_returned_untouched() {
        assert_eq!(
            finish_peer_command("command-codes-fixture-success", Ok::<_, PeerSyncError>(7)),
            Ok(7)
        );
    }
}
