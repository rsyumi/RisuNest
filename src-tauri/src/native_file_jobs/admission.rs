//! Native admission shared by file workers and server sync. No waiting or cancellation.
use std::sync::{Arc, Mutex};
#[derive(Default, Debug)]
pub(crate) struct Admission(Mutex<State>);
#[derive(Default, Debug)]
struct State {
    files: usize,
    exclusive: bool,
    server: bool,
}
#[derive(Debug)]
pub(crate) struct Permit {
    admission: Arc<Admission>,
    kind: Kind,
}
#[derive(Debug)]
enum Kind {
    File(bool),
    Server,
}
impl Admission {
    pub(crate) fn file(self: &Arc<Self>, exclusive: bool) -> Result<Permit, &'static str> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "library-operation-state-unavailable")?;
        if state.server || state.exclusive || (exclusive && state.files != 0) {
            return Err("library-operation-busy");
        }
        state.files += 1;
        state.exclusive = exclusive;
        Ok(Permit {
            admission: self.clone(),
            kind: Kind::File(exclusive),
        })
    }
    pub(crate) fn server(self: &Arc<Self>) -> Result<Permit, &'static str> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "library-operation-state-unavailable")?;
        if state.server || state.files != 0 {
            return Err("library-operation-busy");
        }
        state.server = true;
        Ok(Permit {
            admission: self.clone(),
            kind: Kind::Server,
        })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.admission.0.lock() {
            match self.kind {
                Kind::File(exclusive) => {
                    state.files -= 1;
                    if exclusive {
                        state.exclusive = false;
                    }
                }
                Kind::Server => state.server = false,
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_and_backup_exclude_each_other_until_owner_settles() {
        let admission = Arc::new(Admission::default());
        let server = admission.server().unwrap();
        assert!(admission.file(true).is_err());
        assert!(admission.file(false).is_err());
        // A cancellation flag alone cannot release the owner's permit.
        assert!(admission.file(true).is_err());
        drop(server);
        let backup = admission.file(true).unwrap();
        assert!(admission.server().is_err());
        assert!(admission.file(false).is_err());
        drop(backup);
        assert!(admission.server().is_ok());
    }
    #[test]
    fn backup_excludes_existing_files_but_normal_worker_concurrency_is_preserved() {
        let admission = Arc::new(Admission::default());
        let first = admission.file(false).unwrap();
        let second = admission.file(false).unwrap();
        assert!(admission.file(true).is_err());
        assert!(admission.server().is_err());
        drop(first);
        assert!(admission.file(true).is_err());
        drop(second);
        assert!(admission.file(true).is_ok());
    }
    #[test]
    fn concurrent_claims_have_one_owner() {
        let admission = Arc::new(Admission::default());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|index| {
                let admission = admission.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let claim = if index == 0 {
                        admission.server()
                    } else {
                        admission.file(true)
                    };
                    barrier.wait();
                    claim.is_ok()
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
}
