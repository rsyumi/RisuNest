package io.github.rsyumi.risunest

import kotlinx.coroutines.CompletableDeferred
import java.util.Locale

internal fun androidExportMimeType(filename: String): String = when (
  filename.substringAfterLast('.', "").lowercase(Locale.ROOT)
) {
  "png" -> "image/png"
  "jpg", "jpeg" -> "image/jpeg"
  "webp" -> "image/webp"
  "zip" -> "application/zip"
  "json" -> "application/json"
  "txt" -> "text/plain"
  "risum", "risup", "charx", "risunest", "risudat" -> "application/x-risunest"
  else -> "application/octet-stream"
}

/** Copies may start immediately; only delivery waits for the actual app document. */
internal class AndroidFrontendReady {
  private val ready = CompletableDeferred<Unit>()
  val isReady: Boolean get() = ready.isCompleted && !ready.isCancelled

  fun markReady(): Boolean = ready.complete(Unit)
  fun cancel() = ready.cancel()
  suspend fun await() = ready.await()
}
