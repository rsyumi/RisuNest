package io.github.rsyumi.risunest

import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.util.UUID
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class SafFileBridgeTest {
  @get:Rule val temporaryFolder = TemporaryFolder.builder().assureDeletion().build()

  private fun temporaryDirectory(): File = temporaryFolder.newFolder()

  private fun terminalDestinationRecord(
    requestId: String,
    exportId: String,
    sourceKind: SafDestinationSourceKind,
    publicationPrerequisitesComplete: Boolean,
  ) = SafDestinationRecord(
    requestId = requestId,
    exportId = exportId,
    phase = SafDestinationPhase.SUCCEEDED,
    destinationUri = "content://provider/document/42",
    bytes = 42,
    code = null,
    warningCodes = emptyList(),
    updatedAtMillis = 2_000,
    sourceKind = sourceKind,
    publicationPrerequisitesComplete = publicationPrerequisitesComplete,
  )

  private fun acknowledgeStoredDestination(
    appData: File,
    store: SafDestinationStateStore,
    requestId: String,
  ) = acknowledgeSafDestinationExport(
    requestId,
    store::load,
    { record ->
      requiresRisuSavePublicationProof(appData, record.exportId, record.sourceKind)
    },
    { record ->
      prepareManagedExportAcknowledgement(appData, record.exportId, record.sourceKind)
    },
    store::clear,
  )

  @Test
  fun `multiple sources spool to separate ready tokens without using display names as paths`() = runBlocking {
    val root = temporaryDirectory()
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      nowMillis = { 1_000 },
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
    assertTrue(manifest.contains("\"displayName\":\"first.risudat\""))
    assertTrue(manifest.contains("\"totalBytes\":3"))
    assertFalse(manifest.contains("\"display_name\""))
    assertEquals(
      "{\"format\":\"risunest-android-saf-spool\",\"version\":1," +
        "\"token\":\"11111111-1111-4111-8111-111111111111\",\"createdAtMillis\":1000}",
      root.resolve("11111111-1111-4111-8111-111111111111/ownership.json").readText(),
    )
    assertFalse(root.resolve("11111111-1111-4111-8111-111111111111/source.json.tmp").exists())
  }

  @Test
  fun `content spool persists its import destination for replay`() = runBlocking {
    val root = temporaryDirectory()
    val token = "31313131-3131-4131-8131-313131313131"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString(token) },
    )

    val batch = spoolOpenedFilesOnIo(
      store,
      listOf(TestSafSource("book.lorebook", 2) { ByteArrayInputStream(byteArrayOf(1, 2)) }),
      importDestination = SafContentImportDestination.MODULE,
    )

    assertEquals(SafContentImportDestination.MODULE, batch.ready.single().importDestination)
    assertEquals(SafContentImportDestination.MODULE, store.listReady().single().importDestination)
    assertTrue(root.resolve("$token/source.json").readText().contains("\"importDestination\":\"module\""))
    assertTrue(androidSpoolBatchScript("request", batch).contains("\"importDestination\":\"module\""))
  }

  @Test
  fun `cancellation between fixed-buffer copies removes the owned partial directory`() = runBlocking {
    val root = temporaryDirectory()
    val token = "33333333-3333-4333-8333-333333333333"
    val store = SafSpoolStore(
      root = root,
      bufferBytes = 4,
      atomicPublisher = testAtomicPublisher,
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
    assertFalse(root.resolve(".spooling-$token").exists())
  }

  @Test
  fun `source failure is reported and its partial bytes are removed`() = runBlocking {
    val root = temporaryDirectory()
    val token = "44444444-4444-4444-8444-444444444444"
    val store = SafSpoolStore(
      root = root,
      bufferBytes = 4,
      atomicPublisher = testAtomicPublisher,
    ) { UUID.fromString(token) }
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

    val removed = SafSpoolStore(root, atomicPublisher = testAtomicPublisher).cleanupStale(
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
  fun `stale cleanup uses stable ownership when readiness manifest is truncated`() {
    val root = temporaryDirectory()
    val token = "99999999-9999-4999-8999-999999999999"
    val directory = root.resolve(token)
    directory.mkdirs()
    directory.resolve("ownership.json").writeText(
      "{\"format\":\"risunest-android-saf-spool\",\"version\":1," +
        "\"token\":\"$token\",\"createdAtMillis\":1}",
    )
    directory.resolve("source.json").writeText("{\"token\":")
    directory.resolve("source.risudat").writeBytes(byteArrayOf(1))

    val removed = SafSpoolStore(root, atomicPublisher = testAtomicPublisher).cleanupStale(
      nowMillis = 2_000,
      staleAfterMillis = 100,
    )

    assertEquals(listOf(token), removed)
    assertFalse(directory.exists())
  }

  @Test
  fun `stale cleanup preserves a conflicting tombstone without matching stable ownership`() {
    val root = temporaryDirectory()
    val token = "99999999-9999-4999-8999-999999999999"
    val foreignToken = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    ownedSpool(root, token, token, modifiedAt = 1L)
    val tombstone = root.resolve(".cleanup-$token")
    tombstone.mkdirs()
    tombstone.resolve("ownership.json").writeText(
      "{\"format\":\"risunest-android-saf-spool\",\"version\":1," +
        "\"token\":\"$foreignToken\",\"createdAtMillis\":1}",
    )
    val sentinel = tombstone.resolve("source.risudat")
    sentinel.writeText("preserve-me")

    val removed = SafSpoolStore(root, atomicPublisher = testAtomicPublisher).cleanupStale(
      nowMillis = 2_000,
      staleAfterMillis = 100,
    )

    assertEquals(emptyList<String>(), removed)
    assertTrue(root.resolve(token).isDirectory)
    assertEquals("preserve-me", sentinel.readText())
  }

  @Test
  fun `stale cleanup reclaims an unpublished staging directory without exposing a token`() {
    val root = temporaryDirectory()
    val token = "99999999-9999-4999-8999-999999999999"
    val staging = root.resolve(".spooling-$token")
    staging.mkdirs()
    staging.resolve("source.json").writeText("{\"token\":")
    staging.resolve("source.risudat").writeBytes(byteArrayOf(1))
    staging.setLastModified(1)

    val removed = SafSpoolStore(root, atomicPublisher = testAtomicPublisher).cleanupStale(
      nowMillis = 2_000,
      staleAfterMillis = 100,
    )

    assertEquals(listOf(token), removed)
    assertFalse(staging.exists())
    assertFalse(root.resolve(token).exists())
  }

  @Test
  fun `invalid token factory UUID never creates a spool directory`() {
    val root = temporaryDirectory()
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString("99999999-9999-1999-8999-999999999999") },
    )

    val batch = store.spool(listOf(TestSafSource("invalid.risudat", 1) {
      ByteArrayInputStream(byteArrayOf(1))
    }))

    assertEquals(emptyList<SafSpoolReady>(), batch.ready)
    assertEquals("invalid-token", batch.failures.single().code)
    assertEquals(emptyList<File>(), root.listFiles().orEmpty().toList())
  }

  @Test
  fun `manifest publication always uses a synced sibling temporary`() {
    val root = temporaryDirectory()
    val publications = mutableListOf<Pair<String, String>>()
    val publisher = SafAtomicPublisher { temporary, target ->
      if (temporary.isDirectory) {
        assertTrue(temporary.name.startsWith(".spooling-"))
        assertTrue(temporary.resolve("ownership.json").isFile)
        assertTrue(temporary.resolve("source.risudat").isFile)
        assertTrue(temporary.resolve("source.json").readText().contains("\"state\":\"ready\""))
      } else {
        assertTrue(temporary.name.endsWith(".tmp"))
      }
      publications.add(temporary.name to target.name)
      testAtomicPublisher.publish(temporary, target)
    }
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = publisher,
      tokenFactory = { UUID.fromString("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") },
    )

    val batch = store.spool(listOf(TestSafSource("atomic.risudat", 1) {
      ByteArrayInputStream(byteArrayOf(1))
    }))

    assertEquals(1, batch.ready.size)
    assertEquals(
      listOf(
        "ownership.json.tmp" to "ownership.json",
        "source.json.tmp" to "source.json",
        "source.json.tmp" to "source.json",
        ".spooling-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" to
          "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
      ),
      publications,
    )
  }

  @Test
  fun `ready spools can be replayed without reopening the provider`() {
    val root = temporaryDirectory()
    var opens = 0
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb") },
    )
    val batch = store.spool(listOf(TestSafSource("replay.risudat", 2) {
      opens += 1
      ByteArrayInputStream(byteArrayOf(1, 2))
    }))

    val firstReplay = store.listReady()
    val secondReplay = store.listReady()

    assertEquals(1, opens)
    assertEquals(batch.ready, firstReplay)
    assertEquals(firstReplay, secondReplay)
  }

  @Test
  fun `ready spool can be discarded once without touching unrelated files`() {
    val root = temporaryDirectory()
    val token = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString(token) },
    )
    store.spool(listOf(TestSafSource("declined.risudat", 2) {
      ByteArrayInputStream(byteArrayOf(1, 2))
    }))
    val unrelated = root.resolve("unrelated")
    unrelated.writeText("preserve")

    assertTrue(store.discardReady(token))
    assertFalse(root.resolve(token).exists())
    assertFalse(store.discardReady(token))
    assertFalse(store.discardReady("../unrelated"))
    assertEquals("preserve", unrelated.readText())
  }

  @Test
  fun `destination state survives store reconstruction with bounded identifiers only`() {
    val root = temporaryDirectory()
    val stateFile = root.resolve("android-saf-destination.json")
    val first = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val record = SafDestinationRecord(
      requestId = "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      exportId = "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
      phase = SafDestinationPhase.COPYING,
      destinationUri = "content://provider/document/42",
      bytes = null,
      code = null,
      warningCodes = listOf("android-saf-provider-not-atomic"),
      updatedAtMillis = 1_000,
      sourceKind = SafDestinationSourceKind.SCREENSHOT,
    )

    first.save(record)
    val restored = SafDestinationStateStore(stateFile, testAtomicPublisher).load()

    assertEquals(record, restored)
    assertTrue(stateFile.readText().contains("\"sourceKind\":\"screenshot\""))
    assertFalse(stateFile.readText().contains("/persistent/exports/"))

    val cancelledPicker = record.copy(
      phase = SafDestinationPhase.CANCELLING,
      destinationUri = null,
      updatedAtMillis = 1_001,
    )
    first.save(cancelledPicker)
    assertEquals(cancelledPicker, first.load())
    assertFalse(first.clear(cancelledPicker.requestId))

    val terminal = cancelledPicker.copy(
      phase = SafDestinationPhase.CANCELLED,
      code = "cancelled",
      updatedAtMillis = 1_002,
    )
    first.save(terminal)
    assertTrue(first.clear(terminal.requestId))
    assertNull(first.load())
  }

  @Test
  fun `publication prerequisite proof survives activity and WebView reconstruction`() {
    val root = temporaryDirectory()
    val stateFile = root.resolve("android-saf-destination.json")
    val first = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val terminal = SafDestinationRecord(
      requestId = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
      exportId = "ffffffff-ffff-4fff-8fff-ffffffffffff",
      phase = SafDestinationPhase.SUCCEEDED,
      destinationUri = "content://provider/document/42",
      bytes = 42,
      code = null,
      warningCodes = emptyList(),
      updatedAtMillis = 2_000,
    )
    first.save(terminal)

    val beforeProof = SafDestinationStateStore(stateFile, testAtomicPublisher).load()
    assertEquals(false, beforeProof?.publicationPrerequisitesComplete)
    assertNull(completedSafPublicationPrerequisites(
      terminal,
      "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
      nowMillis = 2_001,
    ))

    val completed = completedSafPublicationPrerequisites(
      beforeProof!!,
      terminal.requestId,
      nowMillis = 2_002,
    )!!
    first.save(completed)
    val afterRecreation = SafDestinationStateStore(stateFile, testAtomicPublisher).load()

    assertEquals(true, afterRecreation?.publicationPrerequisitesComplete)
    assertEquals(2_002L, afterRecreation?.updatedAtMillis)
    assertTrue(stateFile.readText().contains("\"publicationPrerequisitesComplete\":true"))
  }

  @Test
  fun `production acknowledgement rejects and preserves RisuSave without publication proof`() {
    val appData = temporaryDirectory()
    val stateFile = appData.resolve("native-file-jobs/android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val terminal = terminalDestinationRecord(
      requestId = "10101010-1010-4010-8010-101010101010",
      exportId = "20202020-2020-4020-8020-202020202020",
      sourceKind = SafDestinationSourceKind.RISU_SAVE,
      publicationPrerequisitesComplete = false,
    )
    store.save(terminal)

    assertFalse(acknowledgeStoredDestination(appData, store, terminal.requestId))
    assertEquals(terminal, store.load())
  }

  @Test
  fun `production acknowledgement removes proved RisuSave terminal`() {
    val appData = temporaryDirectory()
    val stateFile = appData.resolve("native-file-jobs/android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val terminal = terminalDestinationRecord(
      requestId = "30303030-3030-4030-8030-303030303030",
      exportId = "40404040-4040-4040-8040-404040404040",
      sourceKind = SafDestinationSourceKind.RISU_SAVE,
      publicationPrerequisitesComplete = true,
    )
    store.save(terminal)

    assertTrue(acknowledgeStoredDestination(appData, store, terminal.requestId))
    assertNull(store.load())
  }

  @Test
  fun `production acknowledgement keeps screenshot legacy character and module authority unchanged`() {
    val appData = temporaryDirectory()
    val stateFile = appData.resolve("native-file-jobs/android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val screenshot = terminalDestinationRecord(
      requestId = "50505050-5050-4050-8050-505050505050",
      exportId = "60606060-6060-4060-8060-606060606060",
      sourceKind = SafDestinationSourceKind.SCREENSHOT,
      publicationPrerequisitesComplete = false,
    )
    val screenshotDirectory = appData.resolve(
      "native-file-jobs/screenshot-output/${screenshot.exportId}",
    )
    screenshotDirectory.mkdirs()
    screenshotDirectory.resolve("ownership").writeText(screenshot.exportId)
    screenshotDirectory.resolve("ready").writeText(screenshot.exportId)
    screenshotDirectory.resolve("archive.zip.part").writeBytes(byteArrayOf(1))
    store.save(screenshot)

    fun acknowledge(requestId: String) = acknowledgeStoredDestination(appData, store, requestId)

    assertTrue(acknowledge(screenshot.requestId))
    assertFalse(screenshotDirectory.exists())
    assertNull(store.load())

    val legacy = terminalDestinationRecord(
      requestId = "70707070-7070-4070-8070-707070707070",
      exportId = "80808080-8080-4080-8080-808080808080",
      sourceKind = SafDestinationSourceKind.LEGACY_BACKUP,
      publicationPrerequisitesComplete = false,
    )
    store.save(legacy)
    assertTrue(acknowledge(legacy.requestId))
    assertNull(store.load())

    val handoffs = appData.resolve("native-file-jobs/handoffs").apply { mkdirs() }
    val character = terminalDestinationRecord(
      requestId = "90909090-9090-4090-8090-909090909090",
      exportId = "a0a0a0a0-a0a0-40a0-80a0-a0a0a0a0a0a0",
      sourceKind = SafDestinationSourceKind.RISU_SAVE,
      publicationPrerequisitesComplete = false,
    )
    handoffs.resolve("risu-character-card-${character.exportId}.png").writeBytes(byteArrayOf(1))
    store.save(character)
    assertTrue(acknowledge(character.requestId))
    assertNull(store.load())

    val module = terminalDestinationRecord(
      requestId = "b0b0b0b0-b0b0-40b0-80b0-b0b0b0b0b0b0",
      exportId = "c0c0c0c0-c0c0-40c0-80c0-c0c0c0c0c0c0",
      sourceKind = SafDestinationSourceKind.RISU_SAVE,
      publicationPrerequisitesComplete = false,
    )
    handoffs.resolve("risu-module-${module.exportId}.risum").writeBytes(byteArrayOf(1))
    store.save(module)
    assertTrue(acknowledge(module.requestId))
    assertNull(store.load())
  }

  @Test
  fun `destination state rejects malformed and oversized persistence`() {
    val root = temporaryDirectory()
    val stateFile = root.resolve("android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    stateFile.writeText("{\"requestId\":")
    assertNull(store.load())

    stateFile.writeText("x".repeat(8_193))
    assertNull(store.load())
  }

  @Test
  fun `destination state rejects pre-release schemas instead of inferring missing fields`() {
    val root = temporaryDirectory()
    val stateFile = root.resolve("android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val current = terminalDestinationRecord(
      requestId = "51515151-5151-4151-8151-515151515151",
      exportId = "61616161-6161-4161-8161-616161616161",
      sourceKind = SafDestinationSourceKind.RISU_SAVE,
      publicationPrerequisitesComplete = true,
    )
    store.save(current)
    val currentJson = stateFile.readText()

    stateFile.writeText(currentJson.replace("\"version\":3", "\"version\":1"))
    assertNull(store.load())
    stateFile.writeText(currentJson.replace("\"version\":3", "\"version\":2"))
    assertNull(store.load())
  }

  @Test
  fun `interrupted destination cleanup preserves provider limited warnings`() {
    assertEquals(
      listOf("android-saf-provider-not-atomic"),
      interruptedSafDestinationWarnings { true },
    )
    assertEquals(
      listOf("android-saf-provider-not-atomic", "partial-destination-may-remain"),
      interruptedSafDestinationWarnings { false },
    )
    assertEquals(
      listOf("android-saf-provider-not-atomic", "partial-destination-may-remain"),
      interruptedSafDestinationWarnings { error("provider lost permission") },
    )
  }

  @Test
  fun `destination recovery distinguishes picker restoration partial cleanup and terminal replay`() {
    fun record(phase: SafDestinationPhase) = SafDestinationRecord(
      requestId = "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      exportId = "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
      phase = phase,
      destinationUri = if (phase == SafDestinationPhase.PICKING) {
        null
      } else {
        "content://provider/document/42"
      },
      bytes = null,
      code = null,
      warningCodes = emptyList(),
      updatedAtMillis = 1_000,
    )

    assertEquals(
      SafDestinationRecoveryAction.WAIT_FOR_PICKER,
      decideSafDestinationRecovery(record(SafDestinationPhase.PICKING), true, nowMillis = 1_000),
    )
    assertEquals(
      SafDestinationRecoveryAction.FAIL_INTERRUPTED,
      decideSafDestinationRecovery(record(SafDestinationPhase.PICKING), false, nowMillis = 1_000),
    )
    assertEquals(
      SafDestinationRecoveryAction.CLEAN_PARTIAL,
      decideSafDestinationRecovery(record(SafDestinationPhase.COPYING), true, nowMillis = 1_000),
    )
    assertEquals(
      SafDestinationRecoveryAction.REPLAY_TERMINAL,
      decideSafDestinationRecovery(record(SafDestinationPhase.SUCCEEDED), false, nowMillis = 1_000),
    )
    assertEquals(
      SafDestinationRecoveryAction.FAIL_INTERRUPTED,
      decideSafDestinationRecovery(
        record(SafDestinationPhase.PICKING),
        true,
        nowMillis = 1_000 + DESTINATION_PICKER_STALE_MILLIS,
      ),
    )
    assertEquals(
      SafDestinationRecoveryAction.WAIT_FOR_PICKER,
      decideSafDestinationRecovery(
        record(SafDestinationPhase.CANCELLING).copy(destinationUri = null),
        true,
        nowMillis = 1_000,
      ),
    )
    assertTrue(
      isPendingSafDestinationPicker(
        record(SafDestinationPhase.CANCELLING).copy(destinationUri = null),
      ),
    )
    assertEquals(
      SafDestinationRecoveryAction.FAIL_INTERRUPTED,
      decideSafDestinationRecovery(
        record(SafDestinationPhase.CANCELLING).copy(destinationUri = null),
        true,
        nowMillis = 1_000 + DESTINATION_PICKER_STALE_MILLIS,
      ),
    )

    val cancelling = record(SafDestinationPhase.CANCELLING).copy(destinationUri = null)
    val selectedAfterCancellation = selectedSafDestinationState(
      cancelling,
      cancelling.requestId,
      "content://provider/document/43",
      cancellationRequested = false,
      nowMillis = 1_001,
    )
    assertEquals(SafDestinationPhase.CANCELLING, selectedAfterCancellation?.phase)

    val expired = expiredSafDestinationState(
      record(SafDestinationPhase.PICKING),
      cancelling.requestId,
      nowMillis = 1_000 + DESTINATION_PICKER_STALE_MILLIS,
    )
    assertEquals(SafDestinationPhase.FAILED, expired?.phase)
    assertNull(
      selectedSafDestinationState(
        expired!!,
        expired.requestId,
        "content://provider/document/44",
        cancellationRequested = false,
        nowMillis = expired.updatedAtMillis + 1,
      ),
    )

    val selected = selectedSafDestinationState(
      record(SafDestinationPhase.PICKING),
      cancelling.requestId,
      "content://provider/document/45",
      cancellationRequested = false,
      nowMillis = 1_001,
    )
    assertEquals(SafDestinationPhase.COPYING, selected?.phase)
    assertNull(
      expiredSafDestinationState(
        selected!!,
        selected.requestId,
        nowMillis = 1_000 + DESTINATION_PICKER_STALE_MILLIS,
      ),
    )
  }

  @Test
  fun `destination slot admits exactly one concurrent picker start`() {
    val slot = SafDestinationSlot()
    val winners = AtomicInteger(0)
    val starts = (0 until 16).map {
      Thread {
        if (slot.tryAcquire()) winners.incrementAndGet()
      }
    }

    starts.forEach(Thread::start)
    starts.forEach(Thread::join)

    assertEquals(1, winners.get())
    slot.release()
    assertTrue(slot.tryAcquire())
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
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportById(appData, "99999999-9999-1999-8999-999999999999"))
    assertNull(resolveManagedExportSource(appData, exports.resolve("manual.risudat").path))

    exports.resolve("risusave-$id.lease").delete()
    assertNull(resolveManagedExportSource(appData, source.path))
  }

  @Test
  fun `destination source accepts an owned ready screenshot ZIP without exposing other files`() {
    val appData = temporaryDirectory()
    val id = "99999999-9999-4999-8999-999999999999"
    val owned = appData.resolve("native-file-jobs/screenshot-output/$id")
    owned.mkdirs()
    owned.resolve("ownership").writeText(id)
    owned.resolve("ready").writeText(id)
    val source = owned.resolve("archive.zip.part")
    source.writeBytes(byteArrayOf(1, 2, 3))
    val unrelated = appData.resolve("native-file-jobs/screenshot-output/unrelated/archive.zip.part")
    unrelated.parentFile!!.mkdirs()
    unrelated.writeBytes(byteArrayOf(9))

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, unrelated.path))

    owned.resolve("ready").writeText("different-owner")
    assertNull(resolveManagedExportSource(appData, source.path))
  }

  @Test
  fun `destination source accepts only exact legacy backup handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-backup-$id.bin").apply {
      writeBytes(byteArrayOf(1, 2, 3))
    }
    val unrelated = handoffs.resolve("backup-$id.bin").apply {
      writeBytes(byteArrayOf(9))
    }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(SafDestinationSourceKind.LEGACY_BACKUP, managedExportSourceKind(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts only exact app-owned portable backup handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risunest-backup-$id.risunest")
    source.writeBytes(byteArrayOf(1, 2, 3))
    val outside = appData.resolve("outside/risunest-backup-$id.risunest")
    outside.parentFile!!.mkdirs()
    outside.writeBytes(byteArrayOf(9))
    val unrelated = handoffs.resolve("manual.risunest").apply { writeBytes(byteArrayOf(8)) }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts only exact app-owned raw recovery handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risunest-rescue-$id.risunest-rescue.zip")
    source.writeBytes(byteArrayOf(1, 2, 3))
    val outside = appData.resolve("outside/risunest-rescue-$id.risunest-rescue.zip")
    outside.parentFile!!.mkdirs()
    outside.writeBytes(byteArrayOf(9))
    val unrelated = handoffs.resolve("manual.risunest-rescue.zip").apply {
      writeBytes(byteArrayOf(8))
    }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts only exact app-owned character CharX handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-charx-$id.charx")
    source.writeBytes(byteArrayOf(1, 2, 3))
    val outside = appData.resolve("outside/risu-charx-$id.charx")
    outside.parentFile!!.mkdirs()
    outside.writeBytes(byteArrayOf(9))
    val unrelated = handoffs.resolve("manual.charx").apply { writeBytes(byteArrayOf(8)) }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts only exact app-owned appended JPEG handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-charx-$id.jpeg").apply { writeBytes(byteArrayOf(1)) }
    val outside = appData.resolve("outside/risu-charx-$id.jpeg").apply {
      parentFile!!.mkdirs()
      writeBytes(byteArrayOf(2))
    }
    val unrelated = handoffs.resolve("risu-charx-$id.jpg").apply { writeBytes(byteArrayOf(3)) }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts only exact app-owned JSON character card handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-character-card-$id.json").apply {
      writeBytes(byteArrayOf(1))
    }
    val outside = appData.resolve("outside/risu-character-card-$id.json").apply {
      parentFile!!.mkdirs()
      writeBytes(byteArrayOf(2))
    }
    val unrelated = handoffs.resolve("character-card-$id.json").apply { writeBytes(byteArrayOf(3)) }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, unrelated.path))
  }

  @Test
  fun `destination source accepts exact PNG card and RISUM handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val pngId = "99999999-9999-4999-8999-999999999999"
    val moduleId = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val png = handoffs.resolve("risu-character-card-$pngId.png").apply { writeBytes(byteArrayOf(1)) }
    val module = handoffs.resolve("risu-module-$moduleId.risum").apply { writeBytes(byteArrayOf(2)) }

    assertEquals(png.canonicalFile, resolveManagedExportSource(appData, png.path))
    assertEquals(module.canonicalFile, resolveManagedExportSource(appData, module.path))
    assertEquals(pngId, managedExportId(png))
    assertEquals(moduleId, managedExportId(module))
    assertEquals(png.canonicalFile, resolveManagedExportById(appData, pngId))
    assertEquals(module.canonicalFile, resolveManagedExportById(appData, moduleId))
  }

  @Test
  fun `acknowledged screenshot cleanup removes only the matching owned handoff`() {
    val appData = temporaryDirectory()
    val id = "99999999-9999-4999-8999-999999999999"
    val otherId = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    fun screenshot(owner: String): File {
      val directory = appData.resolve("native-file-jobs/screenshot-output/$owner")
      directory.mkdirs()
      directory.resolve("ownership").writeText(owner)
      directory.resolve("ready").writeText(owner)
      return directory.resolve("archive.zip.part").apply { writeBytes(byteArrayOf(1)) }
    }
    val source = screenshot(id)
    val other = screenshot(otherId)

    assertTrue(discardManagedScreenshotSource(appData, id))
    assertFalse(source.exists())
    assertTrue(other.exists())
    assertFalse(discardManagedScreenshotSource(appData, id))
  }

  @Test
  fun `screenshot acknowledgement requires cleanup before terminal state can be cleared`() {
    val appData = temporaryDirectory()
    val id = "99999999-9999-4999-8999-999999999999"
    val directory = appData.resolve("native-file-jobs/screenshot-output/$id")
    directory.mkdirs()
    directory.resolve("ownership").writeText(id)
    directory.resolve("ready").writeText("different-owner")
    directory.resolve("archive.zip.part").writeBytes(byteArrayOf(1))

    assertFalse(prepareManagedExportAcknowledgement(
      appData,
      id,
      SafDestinationSourceKind.SCREENSHOT,
    ))
    assertTrue(directory.exists())

    directory.resolve("ready").writeText(id)
    assertTrue(prepareManagedExportAcknowledgement(
      appData,
      id,
      SafDestinationSourceKind.SCREENSHOT,
    ))
    assertFalse(directory.exists())
    assertTrue(prepareManagedExportAcknowledgement(
      appData,
      id,
      SafDestinationSourceKind.SCREENSHOT,
    ))
  }

  @Test
  fun `destination picker receives a safe risudat display name`() {
    assertEquals("backup.risudat", safeSafDestinationName("folder/backup.risudat"))
    assertEquals("backup_file.risudat", safeSafDestinationName("backup file"))
    assertEquals("opened-file.risudat", safeSafDestinationName("///"))
    assertEquals("chat.zip", safeSafDestinationName("folder/chat.zip"))
    assertEquals("Leased.charx", safeSafDestinationName("Leased.charx"))
    assertEquals("Leased.jpeg", safeSafDestinationName("Leased.jpeg"))
    assertEquals("Leased.json", safeSafDestinationName("Leased.json"))
    assertEquals("character.png", safeSafDestinationName("character.png"))
    assertEquals("module.risum", safeSafDestinationName("module.risum"))
    assertEquals(
      "risunest-2026-08-29T00-00-00-000Z.risunest",
      safeSafDestinationName("risunest-2026-08-29T00-00-00-000Z.risunest"),
    )
    assertEquals("risu-backup.bin", safeSafDestinationName("risu-backup.bin"))
    assertEquals("backup.risunest", safeSafDestinationName("folder/backup.risunest"))
    assertEquals("BACKUP.RISUNEST", safeSafDestinationName("BACKUP.RISUNEST"))
    val longBackup = "b".repeat(181) + ".risunest"
    assertEquals(180, safeSafDestinationName(longBackup).length)
    assertTrue(safeSafDestinationName(longBackup).endsWith(".risunest"))
  }

  @Test
  fun `destination terminal script keeps a replayable bounded result`() {
    val script = androidSafDestinationScript(
      requestId = "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      exportId = "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
      sourceKind = SafDestinationSourceKind.SCREENSHOT,
      state = "failed",
      code = "destination-interrupted",
      message = "copy interrupted",
      warningCodes = listOf(
        "android-saf-provider-not-atomic",
        "partial-destination-may-remain",
      ),
    )

    assertTrue(script.startsWith("window.tauriAndroidSafDestinationResult="))
    assertTrue(script.contains("risu-android-saf-destination"))
    assertTrue(script.contains("\"exportId\":\"dddddddd-dddd-4ddd-8ddd-dddddddddddd\""))
    assertTrue(script.contains("\"sourceKind\":\"screenshot\""))
    assertFalse(script.contains("content://"))
    assertFalse(script.contains("persistent/exports"))
  }

  private fun ownedSpool(root: File, directoryToken: String, manifestToken: String, modifiedAt: Long) {
    val directory = root.resolve(directoryToken)
    directory.mkdirs()
    directory.resolve("source.risudat").writeBytes(byteArrayOf(1))
    directory.resolve("source.json").writeText(
      "{\"token\":\"$manifestToken\",\"state\":\"copying\",\"displayName\":\"x\",\"bytes\":null,\"totalBytes\":null}",
    )
    directory.resolve("ownership.json").writeText(
      "{\"format\":\"risunest-android-saf-spool\",\"version\":1," +
        "\"token\":\"$manifestToken\",\"createdAtMillis\":$modifiedAt}",
    )
    directory.setLastModified(modifiedAt)
    directory.resolve("source.json").setLastModified(modifiedAt)
  }

  private val testAtomicPublisher = SafAtomicPublisher { temporary, target ->
    Files.move(
      temporary.toPath(),
      target.toPath(),
      StandardCopyOption.ATOMIC_MOVE,
      StandardCopyOption.REPLACE_EXISTING,
    )
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
