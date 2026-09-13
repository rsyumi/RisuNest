// RisuNest local patch. SPDX-License-Identifier: Apache-2.0 OR MIT
package app.tauri.barcodescanner

/** Owns one scan callback and invalidates work from every completed/cancelled scan. */
internal class ScanSession<T> {
    var generation = 0L
        private set
    var pending: T? = null
        private set
    fun begin(value: T): Boolean {
        if (pending != null) return false
        generation++
        pending = value
        return true
    }
    fun isCurrent(ticket: Long): Boolean = ticket == generation && pending != null
    fun finish(): T? {
        val result = pending
        pending = null
        generation++
        return result
    }
}
