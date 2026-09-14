use crate::{Error, Result};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const IDLE_THRESHOLD: Duration = Duration::from_secs(30);
pub const LEASE_DURATION: Duration = Duration::from_secs(45);

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone, Copy)]
pub enum WorkKind {
    Request,
    Background,
}

struct Lease {
    token: String,
    expires: Instant,
}

struct State {
    last_work: Instant,
    active_requests: u64,
    active_background_jobs: u64,
    lease: Option<Lease>,
    stopping: bool,
}

#[derive(Clone)]
pub struct Workload {
    state: Arc<Mutex<State>>,
    clock: Clock,
    idle_threshold: Duration,
    lease_duration: Duration,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadStatus {
    pub state: &'static str,
    pub idle_for_seconds: u64,
    pub idle_threshold_seconds: u64,
    pub idle_eligible: bool,
    pub active_requests: u64,
    pub active_background_jobs: u64,
    pub drained: bool,
    pub lease_expires_in_seconds: Option<u64>,
    pub lease_duration_seconds: u64,
}

pub struct WorkGuard {
    workload: Workload,
    kind: WorkKind,
    performed_work: bool,
}

impl Workload {
    pub fn new() -> Self {
        Self::with_clock(IDLE_THRESHOLD, LEASE_DURATION, Arc::new(Instant::now))
    }

    pub(crate) fn with_clock(
        idle_threshold: Duration,
        lease_duration: Duration,
        clock: Clock,
    ) -> Self {
        let now = clock();
        Self {
            state: Arc::new(Mutex::new(State {
                last_work: now,
                active_requests: 0,
                active_background_jobs: 0,
                lease: None,
                stopping: false,
            })),
            clock,
            idle_threshold,
            lease_duration,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| Error::new("maintenance-state-unavailable", 503))
    }

    fn recover_expired(&self, state: &mut State, now: Instant) {
        if !state.stopping
            && state
                .lease
                .as_ref()
                .is_some_and(|lease| lease.expires <= now)
        {
            state.lease = None;
        }
    }

    pub fn begin(&self, kind: WorkKind) -> Result<WorkGuard> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        if state.stopping || state.lease.is_some() {
            return Err(Error::new("server-updating", 503));
        }
        match kind {
            WorkKind::Request => {
                state.last_work = now;
                state.active_requests += 1;
            }
            WorkKind::Background => state.active_background_jobs += 1,
        }
        Ok(WorkGuard {
            workload: self.clone(),
            kind,
            performed_work: matches!(kind, WorkKind::Request),
        })
    }

    pub fn status(&self) -> Result<WorkloadStatus> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        Ok(self.snapshot(&state, now))
    }

    pub fn acquire(&self) -> Result<(String, WorkloadStatus)> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        if state.lease.is_some() {
            return Err(Error::new("maintenance-already-active", 409));
        }
        if now.saturating_duration_since(state.last_work) < self.idle_threshold {
            return Err(Error::new("maintenance-not-idle", 409));
        }
        let token = random_token()?;
        state.lease = Some(Lease {
            token: token.clone(),
            expires: now + self.lease_duration,
        });
        Ok((token, self.snapshot(&state, now)))
    }

    pub fn renew(&self, token: &str) -> Result<WorkloadStatus> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        if state.stopping {
            return Err(Error::new("maintenance-stopping", 409));
        }
        let lease = state
            .lease
            .as_mut()
            .ok_or(Error::new("maintenance-not-active", 409))?;
        if !constant_time_eq(token, &lease.token) {
            return Err(Error::new("maintenance-lease-denied", 403));
        }
        lease.expires = now + self.lease_duration;
        Ok(self.snapshot(&state, now))
    }

    pub fn release(&self, token: &str) -> Result<WorkloadStatus> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        if state.stopping {
            return Err(Error::new("maintenance-stopping", 409));
        }
        let lease = state
            .lease
            .as_ref()
            .ok_or(Error::new("maintenance-not-active", 409))?;
        if !constant_time_eq(token, &lease.token) {
            return Err(Error::new("maintenance-lease-denied", 403));
        }
        state.lease = None;
        Ok(self.snapshot(&state, now))
    }

    pub fn transition_to_stopping(&self, token: &str) -> Result<WorkloadStatus> {
        let now = (self.clock)();
        let mut state = self.lock()?;
        self.recover_expired(&mut state, now);
        let lease = state
            .lease
            .as_ref()
            .ok_or(Error::new("maintenance-not-active", 409))?;
        if !constant_time_eq(token, &lease.token) {
            return Err(Error::new("maintenance-lease-denied", 403));
        }
        if state.active_requests != 0 || state.active_background_jobs != 0 {
            return Err(Error::new("maintenance-not-drained", 409));
        }
        state.stopping = true;
        Ok(self.snapshot(&state, now))
    }

    fn snapshot(&self, state: &State, now: Instant) -> WorkloadStatus {
        let idle = now.saturating_duration_since(state.last_work);
        let drained = state.active_requests == 0 && state.active_background_jobs == 0;
        WorkloadStatus {
            state: if state.stopping {
                "stopping"
            } else if state.lease.is_some() {
                "draining"
            } else {
                "open"
            },
            idle_for_seconds: idle.as_secs(),
            idle_threshold_seconds: self.idle_threshold.as_secs(),
            idle_eligible: state.lease.is_none() && idle >= self.idle_threshold,
            active_requests: state.active_requests,
            active_background_jobs: state.active_background_jobs,
            drained,
            lease_expires_in_seconds: state
                .lease
                .as_ref()
                .map(|lease| lease.expires.saturating_duration_since(now).as_secs()),
            lease_duration_seconds: self.lease_duration.as_secs(),
        }
    }
}

impl Default for Workload {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        let now = (self.workload.clock)();
        if let Ok(mut state) = self.workload.state.lock() {
            match self.kind {
                WorkKind::Request => state.active_requests -= 1,
                WorkKind::Background => state.active_background_jobs -= 1,
            }
            if self.performed_work {
                state.last_work = now;
            }
        }
    }
}

impl WorkGuard {
    pub fn set_performed_work(&mut self, performed: bool) {
        self.performed_work = performed;
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| Error::new("maintenance-entropy-unavailable", 503))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fixture() -> (Workload, Arc<Mutex<Instant>>) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock_now = now.clone();
        let clock = Arc::new(move || *clock_now.lock().unwrap());
        (
            Workload::with_clock(Duration::from_secs(30), Duration::from_secs(45), clock),
            now,
        )
    }

    fn advance(now: &Mutex<Instant>, duration: Duration) {
        let mut value = now.lock().unwrap();
        *value += duration;
    }

    #[test]
    fn idle_acquire_closes_admission_until_owner_releases() {
        let (workload, now) = fixture();
        advance(&now, Duration::from_secs(30));
        let (token, status) = workload.acquire().unwrap();
        assert_eq!(status.state, "draining");
        let error = workload.begin(WorkKind::Request).err().unwrap();
        assert_eq!((error.code, error.status), ("server-updating", 503));
        workload.release(&token).unwrap();
        assert!(workload.begin(WorkKind::Request).is_ok());
    }

    #[test]
    fn acquisition_closes_admission_while_existing_work_drains() {
        let (workload, now) = fixture();
        let mut guard = workload.begin(WorkKind::Background).unwrap();
        assert_eq!(workload.acquire().unwrap_err().code, "maintenance-not-idle");
        advance(&now, Duration::from_secs(30));
        let (token, draining) = workload.acquire().unwrap();
        assert!(!draining.drained);
        assert_eq!(draining.active_background_jobs, 1);
        assert_eq!(
            workload.begin(WorkKind::Request).err().unwrap().code,
            "server-updating"
        );
        guard.set_performed_work(true);
        drop(guard);
        assert!(workload.status().unwrap().drained);
        workload.release(&token).unwrap();
    }

    #[test]
    fn recent_completed_work_prevents_idle_acquire() {
        let (workload, now) = fixture();
        let mut guard = workload.begin(WorkKind::Background).unwrap();
        assert_eq!(workload.acquire().unwrap_err().code, "maintenance-not-idle");
        guard.set_performed_work(true);
        drop(guard);
        advance(&now, Duration::from_secs(29));
        assert_eq!(workload.acquire().unwrap_err().code, "maintenance-not-idle");
        advance(&now, Duration::from_secs(1));
        assert!(workload.acquire().is_ok());
    }

    #[test]
    fn expired_owner_recovers_admission_and_cannot_release_new_owner() {
        let (workload, now) = fixture();
        advance(&now, Duration::from_secs(30));
        let (expired, _) = workload.acquire().unwrap();
        advance(&now, Duration::from_secs(46));
        assert_eq!(workload.status().unwrap().state, "open");
        let request = workload.begin(WorkKind::Request).unwrap();
        drop(request);
        assert_eq!(
            workload.release(&expired).unwrap_err().code,
            "maintenance-not-active"
        );
    }

    #[test]
    fn renew_and_shutdown_require_the_current_owner_and_a_complete_drain() {
        let (workload, now) = fixture();
        let active = workload.begin(WorkKind::Request).unwrap();
        advance(&now, Duration::from_secs(30));
        let (token, _) = workload.acquire().unwrap();
        assert_eq!(
            workload.renew("0").unwrap_err().code,
            "maintenance-lease-denied"
        );
        advance(&now, Duration::from_secs(20));
        let renewed = workload.renew(&token).unwrap();
        assert_eq!(renewed.lease_expires_in_seconds, Some(45));
        assert_eq!(
            workload.transition_to_stopping(&token).unwrap_err().code,
            "maintenance-not-drained"
        );
        drop(active);
        assert_eq!(
            workload.transition_to_stopping(&token).unwrap().state,
            "stopping"
        );
        advance(&now, Duration::from_secs(46));
        assert_eq!(workload.status().unwrap().state, "stopping");
        assert_eq!(
            workload.begin(WorkKind::Request).err().unwrap().code,
            "server-updating"
        );
    }
}
