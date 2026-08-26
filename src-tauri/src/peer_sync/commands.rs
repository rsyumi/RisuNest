use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    large_fixture_passed: bool,
    production_enabled: bool,
}

impl PeerCloneCapabilities {
    fn current() -> Self {
        Self {
            desktop: true,
            source_ready: false,
            atomic_activation_ready: false,
            large_fixture_passed: false,
            production_enabled: false,
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
                large_fixture_passed: false,
                production_enabled: false,
            }
        );
    }
}
