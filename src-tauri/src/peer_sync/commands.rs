use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
    production_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCloneGateState {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
}

impl PeerCloneCapabilities {
    fn current() -> Self {
        Self::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: false,
            atomic_activation_ready: false,
            lossless_backup_ready: false,
            http_transport_ready: false,
            large_fixture_passed: false,
        })
    }

    fn from_gates(gates: PeerCloneGateState) -> Self {
        Self {
            desktop: gates.desktop,
            source_ready: gates.source_ready,
            atomic_activation_ready: gates.atomic_activation_ready,
            lossless_backup_ready: gates.lossless_backup_ready,
            http_transport_ready: gates.http_transport_ready,
            large_fixture_passed: gates.large_fixture_passed,
            production_enabled: gates.desktop
                && gates.source_ready
                && gates.atomic_activation_ready
                && gates.lossless_backup_ready
                && gates.http_transport_ready,
        }
    }
}

#[tauri::command]
pub fn peer_clone_capabilities() -> PeerCloneCapabilities {
    PeerCloneCapabilities::current()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_lan_clone_stays_closed_until_every_production_gate_passes() {
        assert_eq!(
            peer_clone_capabilities(),
            PeerCloneCapabilities {
                desktop: true,
                source_ready: false,
                atomic_activation_ready: false,
                lossless_backup_ready: false,
                http_transport_ready: false,
                large_fixture_passed: false,
                production_enabled: false,
            }
        );
    }

    #[test]
    fn production_gate_requires_lossless_backup_and_qualified_http_transport() {
        let otherwise_ready = PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: true,
        };

        assert!(PeerCloneCapabilities::from_gates(otherwise_ready).production_enabled);
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                lossless_backup_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                http_transport_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
    }

    #[test]
    fn large_fixture_is_release_evidence_not_a_runtime_gate() {
        let capabilities = PeerCloneCapabilities::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: false,
        });

        assert!(capabilities.production_enabled);
        assert!(!capabilities.large_fixture_passed);
    }
}
