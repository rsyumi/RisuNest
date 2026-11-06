//! Blocking work inside async commands.
//!
//! Tauri runs the body of a synchronous `#[tauri::command(async)]` on an async
//! runtime worker, and the runtime has one worker per core. A body that waits
//! for a lock keeps its worker for the whole wait, so a few of them waiting
//! behind one long holder leave no worker for any other async command.

use tokio::runtime::{Handle, RuntimeFlavor};

/// Runs `work`, which blocks the thread, without holding up other async tasks:
/// on a worker of a multi-thread runtime, the worker's other tasks move to
/// another thread first. Anywhere else `work` just runs.
pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}
