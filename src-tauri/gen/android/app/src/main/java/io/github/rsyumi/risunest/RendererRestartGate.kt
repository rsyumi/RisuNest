package io.github.rsyumi.risunest

/** A dead renderer can be disposed while hidden, but task relaunch waits for resume. */
internal class RendererRestartGate(
  private val post: (() -> Unit) -> Unit,
  private val restart: () -> Unit,
) {
  private var resumed = false
  private var pending = false
  private var queued = false

  fun request(): Boolean {
    pending = true
    schedule()
    return true
  }

  fun onResume() {
    resumed = true
    schedule()
  }

  fun onPause() {
    resumed = false
  }

  fun close() {
    resumed = false
    pending = false
  }

  private fun schedule() {
    if (!resumed || !pending || queued) return
    queued = true
    post {
      queued = false
      if (resumed && pending) {
        pending = false
        restart()
      }
    }
  }
}
