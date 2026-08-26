package co.aiclient.risu

import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.nio.file.Files
import java.util.UUID
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SafFileBridgeTest {
  private fun temporaryDirectory(): File = Files.createTempDirectory("risu-saf-test").toFile()

  @Test
  fun `multiple sources spool to separate ready tokens without using display names as paths`() = runBlocking {
    val root = temporaryDirectory()
    val store = SafSpoolStore(
      root = root,
      tokenFactory = sequenceOf(
        UUID.fromString("11111111-1111-4111-8111-111111111111"),
        UUID.fromString("22222222-2222-4222-8222-222222222222"),
      ).iterator()::next,
    )
    val openedThreads = mutableListOf<String>()
    val batch = spoolOpenedFilesOnIo(
      store,
      listOf(
        TestSafSource("../first.risudat", 3) {
          openedThreads.add(Thread.currentThread().name)
          ByteArrayInputStream(byteArrayOf(1, 2, 3))
        },
        TestSafSource("folder\\second.risudat", null) {
          openedThreads.add(Thread.currentThread().name)
          ByteArrayInputStream(byteArrayOf(4, 5))
        },
      ),
    )

    assertEquals(emptyList<SafSpoolFailure>(), batch.failures)
    assertEquals(
      listOf(
        SafSpoolReady(
          token = "11111111-1111-4111-8111-111111111111",
          displayName = "first.risudat",
          bytes = 3,
          totalBytes = 3,
        ),
        SafSpoolReady(
          token = "22222222-2222-4222-8222-222222222222",
          displayName = "second.risudat",
          bytes = 2,
          totalBytes = null,
        ),
      ),
      batch.ready,
    )
    assertEquals(byteArrayOf(1, 2, 3).toList(), root
      .resolve("11111111-1111-4111-8111-111111111111/source.risudat")
      .readBytes()
      .toList())
    assertEquals(byteArrayOf(4, 5).toList(), root
      .resolve("22222222-2222-4222-8222-222222222222/source.risudat")
      .readBytes()
      .toList())
    assertFalse(root.resolve("first.risudat").exists())
    assertTrue(openedThreads.all { it != Thread.currentThread().name })
    val manifest = root.resolve("11111111-1111-4111-8111-111111111111/source.json").readText()
    assertTrue(manifest.contains("\"state\":\"ready\""))
    assertTrue(manifest.contains("\"display_name\":\"first.risudat\""))
    assertTrue(manifest.contains("\"total_bytes\":3"))
    assertFalse(manifest.contains("\"displayName\""))
  }

  @Test
  fun `cancellation between fixed-buffer copies removes the owned partial directory`() = runBlocking {
    val root = temporaryDirectory()
    val token = "33333333-3333-4333-8333-333333333333"
    val store = SafSpoolStore(
      root = root,
      bufferBytes = 4,
      tokenFactory = { UUID.fromString(token) },
    )
    var copied = 0L

    val batch = spoolOpenedFilesOnIo(
      store,
      listOf(TestSafSource("cancel.risudat", 12) {
        ByteArrayInputStream(ByteArray(12) { it.toByte() })
      }),
      isCancelled = { copied >= 4 },
      onProgress = { progress -> copied = progress.copiedBytes },
    )

    assertEquals(emptyList<SafSpoolReady>(), batch.ready)
    assertEquals("cancelled", batch.failures.single().code)
    assertFalse(root.resolve(token).exists())
  }

  @Test
  fun `source failure is reported and its partial bytes are removed`() = runBlocking {
    val root = temporaryDirectory()
    val token = "44444444-4444-4444-8444-444444444444"
    val store = SafSpoolStore(root, bufferBytes = 4) { UUID.fromString(token) }
    val source = TestSafSource("broken.risudat", null) {
      object : InputStream() {
        private var reads = 0

        override fun read(): Int = error("single-byte read is not used")

        override fun read(bytes: ByteArray, offset: Int, length: Int): Int {
          if (reads++ == 0) {
            bytes[offset] = 7
            return 1
          }
          throw IOException("provider stopped")
        }
      }
    }

    val batch = spoolOpenedFilesOnIo(store, listOf(source))

    assertEquals(emptyList<SafSpoolReady>(), batch.ready)
    assertEquals("source-read-failed", batch.failures.single().code)
    assertFalse(root.resolve(token).exists())
  }

  @Test
  fun `stale cleanup removes only inactive manifest-owned immediate directories`() {
    val root = temporaryDirectory()
    val staleToken = "55555555-5555-4555-8555-555555555555"
    val activeToken = "66666666-6666-4666-8666-666666666666"
    val freshToken = "77777777-7777-4777-8777-777777777777"
    val mismatchedToken = "88888888-8888-4888-8888-888888888888"
    val now = 2_000_000L

    ownedSpool(root, staleToken, staleToken, modifiedAt = 1L)
    ownedSpool(root, activeToken, activeToken, modifiedAt = 1L)
    ownedSpool(root, freshToken, freshToken, modifiedAt = now)
    ownedSpool(root, mismatchedToken, UUID.randomUUID().toString(), modifiedAt = 1L)
    root.resolve("unrelated").mkdirs()

    val removed = SafSpoolStore(root).cleanupStale(
      nowMillis = now,
      staleAfterMillis = 100L,
      activeTokens = setOf(activeToken),
    )

    assertEquals(listOf(staleToken), removed)
    assertFalse(root.resolve(staleToken).exists())
    assertTrue(root.resolve(activeToken).exists())
    assertTrue(root.resolve(freshToken).exists())
    assertTrue(root.resolve(mismatchedToken).exists())
    assertTrue(root.resolve("unrelated").exists())
  }

  @Test
  fun `SAF destination copy reports provider atomicity and partial cleanup honestly`() = runBlocking {
    val root = temporaryDirectory()
    val source = root.resolve("risusave-99999999-9999-4999-8999-999999999999.risudat")
    source.writeBytes(ByteArray(10) { it.toByte() })
    val copied = mutableListOf<Byte>()

    val success = copySafDestinationOnIo(
      source,
      openDestination = { collectingOutput(copied) },
      deletePartial = { true },
      createdDocument = true,
      bufferBytes = 4,
    )

    assertEquals(10, success.bytes)
    assertEquals((0..9).map(Int::toByte), copied)
    assertEquals(listOf("android-saf-provider-not-atomic"), success.warningCodes)

    var deleteAttempts = 0
    val failure = try {
      copySafDestinationOnIo(
        source,
        openDestination = { failingOutput(afterBytes = 4) },
        deletePartial = {
          deleteAttempts += 1
          false
        },
        createdDocument = true,
        bufferBytes = 4,
      )
      null
    } catch (error: SafDestinationException) {
      error
    }

    assertEquals(1, deleteAttempts)
    assertEquals("destination-write-failed", failure?.code)
    assertEquals(
      listOf("android-saf-provider-not-atomic", "partial-destination-may-remain"),
      failure?.warningCodes,
    )
  }

  @Test
  fun `destination source accepts only owned completed exports under the immediate root`() {
    val appData = temporaryDirectory()
    val exports = appData.resolve("persistent/exports")
    exports.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = exports.resolve("risusave-$id.risudat")
    source.writeBytes(byteArrayOf(1))
    exports.resolve("risusave-$id.lease").writeText("{\"exportId\":\"$id\",\"lease\":\"x\"}")
    val outside = appData.resolve("outside/risusave-$id.risudat")
    outside.parentFile!!.mkdirs()
    outside.writeBytes(byteArrayOf(2))

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, exports.resolve("manual.risudat").path))

    exports.resolve("risusave-$id.lease").delete()
    assertNull(resolveManagedExportSource(appData, source.path))
  }

  @Test
  fun `destination picker receives a safe risudat display name`() {
    assertEquals("backup.risudat", safeSafDestinationName("folder/backup.risudat"))
    assertEquals("backup_file.risudat", safeSafDestinationName("backup file"))
    assertEquals("opened-file.risudat", safeSafDestinationName("///"))
  }

  private fun ownedSpool(root: File, directoryToken: String, manifestToken: String, modifiedAt: Long) {
    val directory = root.resolve(directoryToken)
    directory.mkdirs()
    directory.resolve("source.risudat").writeBytes(byteArrayOf(1))
    directory.resolve("source.json").writeText(
      "{\"token\":\"$manifestToken\",\"state\":\"copying\",\"display_name\":\"x\",\"bytes\":null,\"total_bytes\":null}",
    )
    directory.setLastModified(modifiedAt)
    directory.resolve("source.json").setLastModified(modifiedAt)
  }

  private fun collectingOutput(destination: MutableList<Byte>) = object : java.io.OutputStream() {
    override fun write(value: Int) {
      destination.add(value.toByte())
    }

    override fun write(bytes: ByteArray, offset: Int, length: Int) {
      for (index in offset until offset + length) destination.add(bytes[index])
    }
  }

  private fun failingOutput(afterBytes: Int) = object : java.io.OutputStream() {
    private var written = 0

    override fun write(value: Int) {
      if (written >= afterBytes) throw IOException("provider full")
      written += 1
    }

    override fun write(bytes: ByteArray, offset: Int, length: Int) {
      if (written >= afterBytes) throw IOException("provider full")
      written += length
    }
  }
}

private data class TestSafSource(
  override val displayName: String,
  override val totalBytes: Long?,
  val input: () -> InputStream,
) : SafInputSource {
  override fun open(): InputStream = input()
}
