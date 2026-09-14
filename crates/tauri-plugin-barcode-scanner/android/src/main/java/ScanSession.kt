// RisuNest local patch. SPDX-License-Identifier: Apache-2.0 OR MIT
package app.tauri.barcodescanner

import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner

private const val MAX_CONSECUTIVE_FRAME_FAILURES = 3

/** Owns one scan callback and invalidates work from every completed/cancelled scan. */
internal class ScanSession<T> {
    var generation = 0L
        private set
    var pending: T? = null
        private set
    private var consecutiveFrameFailures = 0
    fun begin(value: T): Boolean {
        if (pending != null) return false
        generation++
        pending = value
        consecutiveFrameFailures = 0
        return true
    }
    fun isCurrent(ticket: Long): Boolean = ticket == generation && pending != null
    fun recordFrameSuccess(ticket: Long) {
        if (isCurrent(ticket)) consecutiveFrameFailures = 0
    }
    fun shouldFailAfterFrameError(ticket: Long): Boolean {
        if (!isCurrent(ticket)) return false
        consecutiveFrameFailures++
        return consecutiveFrameFailures >= MAX_CONSECUTIVE_FRAME_FAILURES
    }
    fun finish(): T? {
        val result = pending
        pending = null
        consecutiveFrameFailures = 0
        generation++
        return result
    }
}

internal class ScanLifecycleObserver(
    private val cancelActiveScan: () -> Unit,
) : DefaultLifecycleObserver {
    override fun onStop(owner: LifecycleOwner) {
        onHostStop()
    }

    internal fun onHostStop() {
        cancelActiveScan()
    }
}
