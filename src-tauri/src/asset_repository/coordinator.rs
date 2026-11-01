use std::io;
use std::sync::{Mutex, MutexGuard, OnceLock};

static REPOSITORY_MUTATION: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Repository mutation lock acquisitions on this thread.
    pub(crate) static REPOSITORY_MUTATION_ACQUISITIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn lock_repository_mutation() -> io::Result<MutexGuard<'static, ()>> {
    #[cfg(test)]
    REPOSITORY_MUTATION_ACQUISITIONS.with(|count| count.set(count.get() + 1));
    REPOSITORY_MUTATION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|error| io::Error::other(format!("repository mutation mutex poisoned: {error}")))
}

/// The repository mutation lock when no one holds it.
pub(crate) fn try_lock_repository_mutation() -> io::Result<Option<MutexGuard<'static, ()>>> {
    match REPOSITORY_MUTATION.get_or_init(|| Mutex::new(())).try_lock() {
        Ok(guard) => Ok(Some(guard)),
        Err(std::sync::TryLockError::WouldBlock) => Ok(None),
        Err(std::sync::TryLockError::Poisoned(error)) => {
            Err(io::Error::other(format!("repository mutation mutex poisoned: {error}")))
        }
    }
}
