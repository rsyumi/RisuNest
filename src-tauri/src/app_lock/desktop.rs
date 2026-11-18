use super::{CoverPolicy, LifecycleSnapshot};

pub(super) struct Lifecycle {
    pub policy: CoverPolicy,
    active: bool,
    epoch: u64,
    departed_at_ms: Option<u64>,
    pending: bool,
    ready_generation: u64,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self { policy: CoverPolicy::default(), active: true, epoch: 0, departed_at_ms: None, pending: false, ready_generation: 0 }
    }
}

impl Lifecycle {
    pub fn snapshot(&self, now: u64) -> LifecycleSnapshot {
        LifecycleSnapshot { active: self.active, epoch: self.epoch, at_ms: now, departed_at_ms: self.departed_at_ms, unconfirmed_release: None }
    }

    pub fn transition(&mut self, active: bool, now: u64) -> Option<LifecycleSnapshot> {
        if active == self.active { return None; }
        self.active = active;
        self.epoch = self.epoch.wrapping_add(1);
        if !active {
            self.departed_at_ms = Some(now);
            if self.policy.installs_return_cover() && !self.policy.gate_required {
                self.pending = true;
            }
        }
        Some(self.snapshot(now))
    }

    pub fn set_policy(&mut self, policy: CoverPolicy) {
        if policy.gate_required != self.policy.gate_required {
            self.ready_generation = self.ready_generation.wrapping_add(1);
        }
        if policy.gate_required && !self.policy.gate_required { self.pending = true; }
        self.policy = policy;
    }

    pub fn covered(&self) -> bool { self.pending }

    pub fn begin_ready(&mut self, epoch: u64, locked: bool) -> Option<u64> {
        if epoch != self.epoch || !self.active { return None; }
        if !locked && self.policy.gate_required { return None; }
        self.ready_generation = self.ready_generation.wrapping_add(1);
        Some(self.ready_generation)
    }

    pub fn finish_ready(&mut self, epoch: u64, generation: u64, locked: bool) -> bool {
        if epoch != self.epoch || !self.active || generation != self.ready_generation || (!locked && self.policy.gate_required) { return false; }
        self.policy.gate_required = locked;
        self.pending = false;
        true
    }

    #[cfg(test)]
    fn ready(&mut self, epoch: u64, locked: bool) -> bool {
        self.begin_ready(epoch, locked).is_some_and(|generation| self.finish_ready(epoch, generation, locked))
    }
}

pub(super) fn color_rgb(value: &str) -> (u8, u8, u8) {
    let Some(hex) = value.strip_prefix('#') else { return (31, 31, 31); };
    if hex.len() != 6 || !hex.bytes().all(|ch| ch.is_ascii_hexdigit()) { return (31, 31, 31); }
    match u32::from_str_radix(hex, 16) {
        Ok(rgb) => ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
        Err(_) => (31, 31, 31),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_lock::LockTiming;

    #[test]
    fn departure_cover_survives_policy_updates_and_stale_readiness() {
        let mut state = Lifecycle::default();
        state.set_policy(CoverPolicy { enabled: true, timing: LockTiming::Immediate, ..Default::default() });
        let departure = state.transition(false, 10).unwrap();
        assert!(state.covered());
        assert!(!state.ready(departure.epoch, false));
        let arrival = state.transition(true, 60_010).unwrap();
        assert_eq!(arrival.departed_at_ms, Some(10));
        state.set_policy(CoverPolicy { enabled: true, timing: LockTiming::Immediate, ..Default::default() });
        assert!(!state.ready(departure.epoch, false));
        assert!(state.covered());
        state.set_policy(CoverPolicy { enabled: true, timing: LockTiming::Immediate, gate_required: true, ..Default::default() });
        assert!(state.ready(arrival.epoch, true));
        assert!(!state.covered());
        assert!(state.policy.gate_required);
    }

    #[test]
    fn duplicate_focus_and_launch_mode_do_not_create_extra_locks() {
        let mut state = Lifecycle::default();
        state.set_policy(CoverPolicy { enabled: true, ..Default::default() });
        assert!(state.transition(true, 1).is_none());
        assert_eq!(state.transition(false, 10).unwrap().epoch, 1);
        assert!(state.transition(false, 20).is_none());
        assert!(!state.covered());
        assert_eq!(state.transition(true, 30).unwrap().epoch, 2);
        state.transition(false, 40);
        assert_eq!(state.snapshot(50).departed_at_ms, Some(40));
    }

    #[test]
    fn newly_required_gate_is_covered_until_its_frame_is_ready() {
        let mut state = Lifecycle::default();
        state.set_policy(CoverPolicy { enabled: true, gate_required: true, ..Default::default() });
        assert!(state.covered());
        assert!(state.ready(0, true));
        state.set_policy(state.policy.clone());
        assert!(!state.covered());
    }

    #[test]
    fn stale_capture_cannot_release_a_new_gate_or_overtake_another_capture() {
        let mut state = Lifecycle::default();
        let before_gate = state.begin_ready(0, false).unwrap();
        state.set_policy(CoverPolicy { enabled: true, gate_required: true, ..Default::default() });
        assert!(!state.finish_ready(0, before_gate, false));
        assert!(state.covered());
        assert!(state.begin_ready(0, false).is_none());
        let previous = state.begin_ready(0, true).unwrap();
        let current = state.begin_ready(0, true).unwrap();
        assert!(!state.finish_ready(0, previous, true));
        assert!(state.covered());
        assert!(state.finish_ready(0, current, true));
        assert!(!state.covered());
    }

    #[test]
    fn safe_error_frame_can_be_shown_after_a_policy_bridge_failure() {
        let mut state = Lifecycle::default();
        state.set_policy(CoverPolicy { enabled: true, timing: LockTiming::Immediate, ..Default::default() });
        state.transition(false, 10);
        let arrival = state.transition(true, 20).unwrap();
        let capture = state.begin_ready(arrival.epoch, true).unwrap();
        assert!(state.finish_ready(arrival.epoch, capture, true));
        assert!(!state.covered());
        assert!(state.policy.gate_required);
    }
}
