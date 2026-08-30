use std::io;
use std::sync::{Mutex, MutexGuard, OnceLock};

static REPOSITORY_MUTATION: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn lock_repository_mutation() -> io::Result<MutexGuard<'static, ()>> {
    REPOSITORY_MUTATION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|error| io::Error::other(format!("repository mutation mutex poisoned: {error}")))
}
