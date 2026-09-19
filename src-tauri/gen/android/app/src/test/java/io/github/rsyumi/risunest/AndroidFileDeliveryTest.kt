package io.github.rsyumi.risunest

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidFileDeliveryTest {
  @Test fun exportMimeMatchesTheActualFilenameWithoutOverclaimingUnknownFiles() {
    val cases = mapOf(
      "character.PNG" to "image/png", "photo.jpeg" to "image/jpeg",
      "image.webp" to "image/webp", "chat.zip" to "application/zip",
      "character.charx" to "application/x-risunest", "module.risum" to "application/x-risunest",
      "backup.risunest" to "application/x-risunest", "backup.risudat" to "application/x-risunest",
      "character.json" to "application/json", "chat.txt" to "text/plain",
      "backup.bin" to "application/octet-stream", "archive.zip.bin" to "application/octet-stream",
      "unknown" to "application/octet-stream",
    )
    for ((name, mime) in cases) assertEquals(name, mime, androidExportMimeType(name))
  }

  @Test fun preparedFileDeliveryWaitsForTheActualFrontendInsteadOfTheInitialDocument() = runBlocking {
    val ready = AndroidFrontendReady()
    val delivered = mutableListOf<String>()
    val first = launch(start = CoroutineStart.UNDISPATCHED) { ready.await(); delivered.add("first") }
    val second = launch(start = CoroutineStart.UNDISPATCHED) { ready.await(); delivered.add("second") }
    assertTrue(delivered.isEmpty())
    assertFalse(ready.isReady)
    assertTrue(ready.markReady())
    assertFalse(ready.markReady())
    first.join()
    second.join()
    assertEquals(listOf("first", "second"), delivered)
    ready.await()
    assertTrue(ready.isReady)
  }

  @Test fun retiredViewCannotReceiveLateFilesOrARevivedReadySignal() = runBlocking {
    val ready = AndroidFrontendReady()
    var delivered = false
    val job = launch(start = CoroutineStart.UNDISPATCHED) { ready.await(); delivered = true }
    ready.cancel()
    assertFalse(ready.markReady())
    job.join()
    assertTrue(job.isCancelled)
    assertFalse(delivered)
    assertFalse(ready.isReady)
  }
}
