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

  @Test
  fun `directory failures return a failed batch without opening the source`() {
    for (failRoot in listOf(true, false)) {
      val root = temporaryDirectory().resolve("sources")
      val publisher = object : SafAtomicPublisher {
        override fun createDirectory(directory: File, parents: Boolean) {
          if (parents == failRoot) throw IOException("synthetic filesystem failure")
          super.createDirectory(directory, parents)
        }
        override fun publish(temporary: File, target: File) = testAtomicPublisher.publish(temporary, target)
      }
      val source = TestSafSource("backup.bin", 3L) { error("Provider must not open before staging exists") }
      val store = SafSpoolStore(root, atomicPublisher = publisher)
      val result = store.spool(listOf(source))
      assertTrue(result.ready.isEmpty())
      assertEquals("spool-write-failed", result.failures.single().code)
      assertTrue(store.listReady().isEmpty())
      assertTrue(root.listFiles().orEmpty().isEmpty())
    }
  }
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
  fun `renderer reconciliation protects live exports and retains copied destinations`() {
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val record = terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false)
    val session = SafDestinationRendererSession()
    session.own(id)
    assertNull(reconciledSafDestination(record, session.owns(id), 3_000, true))
    val oldGeneration = session.generation
    session.retire()
    session.own(id, oldGeneration)
    assertFalse(session.owns(id))
    val recovered = reconciledSafDestination(record, session.owns(id), 3_000, true)!!
    assertEquals(SafDestinationPhase.FAILED, recovered.phase)
    assertEquals("destination-interrupted", recovered.code)
    assertEquals(record.destinationUri, recovered.destinationUri)
    assertTrue(recovered.publicationPrerequisitesComplete)
    assertTrue(recovered.warningCodes.contains("partial-destination-may-remain"))
    assertEquals(record.copy(publicationPrerequisitesComplete = true),
      reconciledSafDestination(record.copy(publicationPrerequisitesComplete = true), false, 3_000, true))
    for (phase in listOf(SafDestinationPhase.CANCELLED, SafDestinationPhase.FAILED)) {
      val unstarted = record.copy(phase = phase, copyStarted = false, exportBound = false, warningCodes = emptyList())
      assertEquals(unstarted, reconciledSafDestination(unstarted, false, 3_000, true))
    }
    assertNull(reconciledSafDestination(record.copy(phase = SafDestinationPhase.COPYING), false, 3_000, true))
    assertNull(reconciledSafDestination(record.copy(sourceKind = SafDestinationSourceKind.SCREENSHOT), false, 3_000, true))
    val fullWarnings = record.copy(warningCodes = (1..16).map { "warning-$it" })
    val bounded = reconciledSafDestination(fullWarnings, false, 3_000, true)!!
    assertEquals(16, bounded.warningCodes.size)
    assertTrue(bounded.warningCodes.contains("partial-destination-may-remain"))
  }

  @Test
  fun `reconciliation preserves portable outcomes while their handoff resolves`() {
    val root = temporaryDirectory()
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val source = root.resolve("native-file-jobs/handoffs/risunest-backup-$id.risunest")
    source.parentFile!!.mkdirs()
    source.writeBytes(byteArrayOf(1, 2, 3))
    val store = SafDestinationStateStore(root.resolve("state.json"), testAtomicPublisher)
    for (phase in listOf(SafDestinationPhase.SUCCEEDED, SafDestinationPhase.FAILED, SafDestinationPhase.CANCELLED)) {
      val record = terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false).copy(
        phase = phase,
        code = if (phase == SafDestinationPhase.SUCCEEDED) null else "destination-write-failed",
        warningCodes = if (phase == SafDestinationPhase.SUCCEEDED) emptyList() else listOf("partial-destination-may-remain"),
      )
      store.save(record)
      val session = SafDestinationRendererSession()
      val recovered = session.reconcile(record, 3_000, store::save, { true },
        { requiresRisuSavePublicationProof(root, it.exportId, it.sourceKind) })!!
      assertEquals(record.copy(publicationPrerequisitesComplete = true, updatedAtMillis = 3_000), recovered)
      assertEquals(recovered, session.status(null))
      assertTrue(source.exists())
      assertTrue(session.acknowledge(id, store.load(), store::clear))
      assertNull(session.status(null))
    }
    assertTrue(source.delete())
    val record = terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false)
    val unresolved = reconciledSafDestination(record, false, 3_000,
      requiresRisuSavePublicationProof(root, id, record.sourceKind))!!
    assertEquals(SafDestinationPhase.FAILED, unresolved.phase)
    assertEquals("destination-interrupted", unresolved.code)
    assertTrue(unresolved.warningCodes.contains("partial-destination-may-remain"))
  }

  @Test
  fun `released 1_0_5 terminal records recover without a copyStarted field`() {
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val file = temporaryDirectory().resolve("state.json")
    val store = SafDestinationStateStore(file, testAtomicPublisher)
    store.save(terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false))
    file.writeText(file.readText().replace("\"copyStarted\":true,", ""))
    val record = store.load()!!
    assertTrue(record.copyStarted)
    val recovered = reconciledSafDestination(record, false, 3_000, true)!!
    store.save(recovered)
    assertEquals("destination-interrupted", store.load()!!.code)
    assertTrue(store.load()!!.publicationPrerequisitesComplete)
  }

  @Test
  fun `recovered receipts stay journaled until acknowledged and cannot acknowledge a newer live export`() {
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val nextId = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    val file = temporaryDirectory().resolve("state.json")
    val store = SafDestinationStateStore(file, testAtomicPublisher)
    val session = SafDestinationRendererSession()
    store.save(terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false))
    val recovered = session.reconcile(store.load()!!, 3_000, store::save, { true }, { true })!!
    assertEquals(recovered, store.load())
    assertEquals(listOf(recovered), session.pending())
    assertFalse(session.acknowledge(id, store.load()) { false })
    assertTrue(session.hasRecovered(id))
    assertEquals(recovered, store.load())
    assertNull(session.reconcile(store.load()!!, 3_001, store::save, { true }, { true }))
    assertEquals(recovered, store.load())
    assertEquals(recovered, session.status(store.load()))
    // A process that dies before acknowledgement recovers the same receipt again.
    assertEquals(recovered, SafDestinationRendererSession().reconcile(store.load()!!, 3_002, store::save, { true }, { true }))
    val next = terminalDestinationRecord(nextId, nextId, SafDestinationSourceKind.RISU_SAVE, false)
    assertNull(session.status(next.copy(phase = SafDestinationPhase.COPYING)))
    assertEquals(next, session.status(next))
    val receipts = SafDestinationReceiptStore(file.resolveSibling("receipts"), testAtomicPublisher)
    keepRecoveredSafReceipt(store.load(), nextId, session, receipts)
    store.save(next)
    session.own(nextId)
    // A process that dies after the newer export took the journal still recovers the first receipt.
    val restarted = SafDestinationRendererSession()
    assertEquals(listOf(recovered), receipts.loadAll().mapNotNull { restarted.reconcile(it, 3_003, receipts::save, { true }, { true }) })
    assertNull(session.reconcile(next, 3_002, store::save, { true }, { true }))
    assertTrue(receipts.clear(id) && session.acknowledge(id, store.load(), store::clear))
    assertTrue(receipts.loadAll().isEmpty())
    assertEquals(next, store.load())
    assertTrue(session.pending().isEmpty())
    assertFalse(session.acknowledge(id, store.load(), store::clear))
  }

  @Test
  fun `failed reconciliation keeps its journal and retries cleanup`() {
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val file = temporaryDirectory().resolve("state.json")
    val store = SafDestinationStateStore(file, testAtomicPublisher)
    val session = SafDestinationRendererSession()
    store.save(terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE, false))
    assertNull(session.reconcile(store.load()!!, 3_000, store::save, { false }, { true }))
    assertTrue(store.load()!!.publicationPrerequisitesComplete)
    assertTrue(session.pending().isEmpty())
    assertTrue(session.reconcile(store.load()!!, 3_000, store::save, { true }, { true }) != null)
    assertEquals(session.pending().single(), store.load())
  }

  @Test
  fun `recovered RisuSave cleanup removes the exact managed source and lease only`() {
    val root = temporaryDirectory()
    val id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val source = root.resolve("persistent/exports/risusave-$id.risudat")
    source.parentFile!!.mkdirs()
    val lease = source.parentFile!!.resolve("risusave-$id.lease")
    val unrelated = source.parentFile!!.resolve("unrelated.risudat").apply { writeText("keep") }
    source.writeText("synthetic")
    lease.writeText("{\"exportId\":\"wrong\"}")
    assertFalse(prepareRecoveredRisuSaveAcknowledgement(root, id))
    assertTrue(source.exists())
    lease.writeText("{\"exportId\":\"$id\"}")
    assertTrue(prepareRecoveredRisuSaveAcknowledgement(root, id))
    assertFalse(source.exists())
    assertFalse(lease.exists())
    assertTrue(unrelated.exists())
    assertTrue(prepareRecoveredRisuSaveAcknowledgement(root, id))
    lease.writeText("{\"exportId\":\"$id\"}")
    assertTrue(prepareRecoveredRisuSaveAcknowledgement(root, id))
    assertFalse(lease.exists())
  }

  @Test
  fun `interrupted preparation has no ready identity and can retry`() {
    val root = temporaryDirectory()
    var cancelled = true
    val source = object : SafInputSource {
      override val operationId = "b".repeat(64)
      override val displayName = "retry.charx"
      override val totalBytes = 3L
      override fun open(): InputStream = ByteArrayInputStream(byteArrayOf(1, 2, 3))
    }
    val store = SafSpoolStore(root, atomicPublisher = testAtomicPublisher)
    assertTrue(store.spool(listOf(source), isCancelled = { cancelled }).ready.isEmpty())
    assertTrue(store.listReady().isEmpty())
    cancelled = false
    assertEquals("b".repeat(64), store.spool(listOf(source), isCancelled = { cancelled }).ready.single().operationId)
  }

  @Test
  fun `ready opened source identity replays without reopening provider`() {
    val root = temporaryDirectory()
    var opened = 0
    val source = object : SafInputSource {
      override val operationId = "a".repeat(64)
      override val displayName = "한글 \"e\u0301\" 😀.charx"
      override val totalBytes = 3L
      override fun open(): InputStream { opened++; return ByteArrayInputStream(byteArrayOf(1, 2, 3)) }
    }
    val first = SafSpoolStore(root, atomicPublisher = testAtomicPublisher).spool(listOf(source))
    val recreated = SafSpoolStore(root, atomicPublisher = testAtomicPublisher)
    assertTrue(recreated.markDelivered(first.ready.single().token))
    val second = recreated.spool(listOf(source))
    assertEquals(1, opened)
    assertEquals(first.ready, second.ready)
    assertEquals("한글 \"é\" 😀.charx", second.ready.single().displayName)
    assertFalse(recreated.isDelivered(second.ready.single().token))
  }

  @Test
  fun `display name keeps Unicode and strips traversal controls within codepoint bound`() {
    assertEquals("opened-file", safeSafDisplayName(".."))
    assertEquals("opened-file", safeSafDisplayName("."))
    assertEquals("이름é😀.CHARX", safeSafDisplayName("folder/이름e\u0301\u202e😀\u0000.CHARX"))
    val bounded = safeSafDisplayName("😀".repeat(200) + ".charx")
    assertEquals(180, bounded.codePointCount(0, bounded.length))
    assertTrue(bounded.endsWith(".charx"))
    assertFalse(Character.isHighSurrogate(bounded[bounded.length - 7]))
  }

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
  fun `a spool an earlier WebView received is discarded and reported instead of replayed`() {
    val root = temporaryDirectory()
    val received = "11111111-1111-4111-8111-111111111111"
    val waiting = "22222222-2222-4222-8222-222222222222"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = sequenceOf(UUID.fromString(received), UUID.fromString(waiting)).iterator()::next,
    )
    store.spool(listOf(
      TestSafSource("large.json", 2) { ByteArrayInputStream(byteArrayOf(1, 2)) },
      TestSafSource("next.json", 2) { ByteArrayInputStream(byteArrayOf(3, 4)) },
    ))
    val handedOff = markSpoolBatchDelivered(store, SafSpoolBatch(store.listReady().take(1), emptyList()))
    assertEquals(listOf(received), handedOff.ready.map(SafSpoolReady::token))

    val restarted = SafSpoolStore(root, atomicPublisher = testAtomicPublisher)
    val replay = replayReadySpoolBatch(restarted, emptySet())

    assertEquals(listOf(waiting), replay.ready.map(SafSpoolReady::token))
    assertEquals(listOf(SafSpoolFailure("large.json", SPOOL_INTERRUPTED_CODE)), replay.failures)
    assertFalse(root.resolve(received).exists())
    assertEquals(SafSpoolBatch(replay.ready, emptyList()), replayReadySpoolBatch(restarted, emptySet()))
  }

  @Test
  fun `a backup copy handed to its request is published already received and never replayed as a restore`() {
    val root = temporaryDirectory()
    val token = "77777777-7777-4777-8777-777777777777"
    val publishedMarked = mutableListOf<Boolean>()
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = SafAtomicPublisher { temporary, target ->
        if (target.name == token) publishedMarked += temporary.resolve("delivered").isFile
        testAtomicPublisher.publish(temporary, target)
      },
      tokenFactory = { UUID.fromString(token) },
    )

    val copied = store.spool(
      listOf(TestSafSource("backup.risudat", 2) { ByteArrayInputStream(byteArrayOf(1, 2)) }),
      restoreCopy = true,
    )

    // Process death right after publication must not leave an unreceived spool.
    assertEquals(listOf(true), publishedMarked)
    assertEquals(listOf(token), copied.ready.map(SafSpoolReady::token))
    val replay = replayReadySpoolBatch(SafSpoolStore(root, atomicPublisher = testAtomicPublisher), emptySet())
    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replay)
    assertFalse(root.resolve(token).exists())
  }

  @Test
  fun `an abandoned restore copy reports failed cleanup and retries without another interrupted notice`() {
    val root = temporaryDirectory()
    var cleanupFails = false
    val store = SafSpoolStore(root, atomicPublisher = SafAtomicPublisher { source, target ->
      if (cleanupFails && target.name.startsWith(".cleanup-")) throw IOException("synthetic cleanup failure")
      testAtomicPublisher.publish(source, target)
    })
    val copy = store.spool(
      listOf(TestSafSource("backup.bin", 2) { ByteArrayInputStream(byteArrayOf(1, 2)) }),
      delivered = true, restoreCopy = true,
    ).ready.single()
    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replayReadySpoolBatch(store, setOf(copy.token)))
    assertTrue(root.resolve(copy.token).exists())
    cleanupFails = true
    val failedCleanup = replayReadySpoolBatch(store, emptySet())
    assertTrue(failedCleanup.ready.isEmpty())
    assertEquals(listOf("cleanup-failed"), failedCleanup.failures.map { it.code })
    assertTrue(root.resolve(copy.token).exists())
    cleanupFails = false
    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replayReadySpoolBatch(store, emptySet()))
    assertFalse(root.resolve(copy.token).exists())
  }

  @Test
  fun `a backup copy cancelled or changed after its last byte never reaches the page`() {
    val root = temporaryDirectory()
    val store = SafSpoolStore(root, atomicPublisher = testAtomicPublisher)
    fun copy() = store.spool(listOf(TestSafSource("backup.risudat", 1) { ByteArrayInputStream(byteArrayOf(1)) }), delivered = true)

    val kept = copy()
    assertEquals(kept, settlePortableSourceCopy(store, kept, verified = { true }, cancelled = { false }))
    val cancelled = copy()
    assertEquals(
      portableSourceCopyFailure("backup.risudat", "cancelled"),
      settlePortableSourceCopy(store, cancelled, verified = { error("A cancelled copy needs no verification") }, cancelled = { true }),
    )
    val changed = copy()
    assertEquals(
      portableSourceCopyFailure("backup.risudat", "source-changed"),
      settlePortableSourceCopy(store, changed, verified = { false }, cancelled = { false }),
    )
    assertEquals(listOf(kept.ready.single().token), store.listReady().map(SafSpoolReady::token))
    // A copy whose spool is already gone reports the cleanup failure instead of a token the page cannot use.
    assertEquals(
      portableSourceCopyFailure("backup.risudat", "cleanup-failed"),
      withdrawPortableSourceCopy(store, cancelled, "cancelled"),
    )
  }

  @Test
  fun `replay leaves a spool the current WebView already received`() {
    val root = temporaryDirectory()
    val token = "33333333-3333-4333-8333-333333333333"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString(token) },
    )
    val batch = store.spool(listOf(TestSafSource("card.lorebook", 1) { ByteArrayInputStream(byteArrayOf(1)) }))
    markSpoolBatchDelivered(store, batch)

    val replay = replayReadySpoolBatch(store, setOf(token))

    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replay)
    assertTrue(store.isDelivered(token))
  }

  @Test
  fun `an opened intent reserves each spool before a replay can offer it`() {
    val root = temporaryDirectory()
    val token = "44444444-4444-4444-8444-444444444444"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      tokenFactory = { UUID.fromString(token) },
    )
    val source = object : SafInputSource {
      override val operationId = "c".repeat(64)
      override val displayName = "card.json"
      override val totalBytes = 1L
      override fun open(): InputStream = ByteArrayInputStream(byteArrayOf(1))
    }
    val reserved = mutableSetOf<String>()
    val offeredAtReservation = mutableListOf<Boolean>()
    fun share() = store.spool(listOf(source), beforePublish = { reservedToken ->
      offeredAtReservation += store.listReady().any { it.token == reservedToken && !store.isDelivered(it.token) }
      reserved += reservedToken
    })

    val first = share()
    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replayReadySpoolBatch(store, reserved))
    markSpoolBatchDelivered(store, first)
    // Opening the same source again reserves it before its earlier delivery is forgotten.
    assertEquals(first.ready, share().ready)
    assertEquals(listOf(false, false), offeredAtReservation)
    assertEquals(SafSpoolBatch(emptyList(), emptyList()), replayReadySpoolBatch(store, reserved))

    reserved.clear()
    assertEquals(first.ready, replayReadySpoolBatch(store, reserved).ready)
  }

  @Test
  fun `a spool that cannot be marked is withdrawn and reported`() {
    val root = temporaryDirectory()
    val store = SafSpoolStore(root, atomicPublisher = testAtomicPublisher)
    val missing = SafSpoolReady("44444444-4444-4444-8444-444444444444", "gone.json", 1, 1)

    val batch = markSpoolBatchDelivered(
      store,
      SafSpoolBatch(listOf(missing), listOf(SafSpoolFailure("other.json", "source-copy-failed"))),
    )

    assertEquals(emptyList<SafSpoolReady>(), batch.ready)
    assertEquals(
      listOf(
        SafSpoolFailure("other.json", "source-copy-failed"),
        SafSpoolFailure("gone.json", "spool-write-failed"),
      ),
      batch.failures,
    )
  }

  @Test
  fun `delivered spools can still be discarded and swept as stale`() {
    val root = temporaryDirectory()
    val discarded = "55555555-5555-4555-8555-555555555555"
    val stale = "66666666-6666-4666-8666-666666666666"
    val store = SafSpoolStore(
      root = root,
      atomicPublisher = testAtomicPublisher,
      nowMillis = { 1_000 },
      tokenFactory = sequenceOf(UUID.fromString(discarded), UUID.fromString(stale)).iterator()::next,
    )
    store.spool(listOf(
      TestSafSource("first.json", 1) { ByteArrayInputStream(byteArrayOf(1)) },
      TestSafSource("second.json", 1) { ByteArrayInputStream(byteArrayOf(2)) },
    ))
    assertTrue(store.markDelivered(discarded))
    assertTrue(store.markDelivered(stale))

    assertTrue(store.discardReady(discarded))
    assertEquals(listOf(stale), store.cleanupStale(nowMillis = 1_000 + 25 * 60 * 60 * 1_000L))
    assertEquals(emptyList<String>(), root.list().orEmpty().toList())
  }

  @Test
  fun `reserved destination binds one job and preserves the distinct handoff identity after restart`() {
    val requestId = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val jobId = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    val sourceId = "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
    val file = temporaryDirectory().resolve("destination.json")
    val store = SafDestinationStateStore(file, testAtomicPublisher)
    val picking = SafDestinationRecord(requestId, requestId, SafDestinationPhase.PICKING,
      null, null, null, emptyList(), 1_000, reservedDestination = true, exportBound = false, copyStarted = false)
    store.save(picking)
    assertNull(bindReservedSafDestination(picking, requestId, jobId, 1_001))
    val selected = selectedSafDestinationState(store.load()!!, requestId,
      "content://synthetic/new-document", false, 1_002)!!
    val bound = bindReservedSafDestination(selected, requestId, jobId, 1_003)!!
    store.save(bound)
    val restarted = SafDestinationStateStore(file, testAtomicPublisher).load()!!
    assertEquals(jobId, restarted.jobId)
    assertNull(bindReservedSafDestination(restarted, requestId, sourceId, 1_004))
    assertNull(beginReservedSafCopy(restarted, sourceId, jobId, sourceId, SafDestinationSourceKind.RISU_SAVE, 1_004))
    assertNull(beginReservedSafCopy(restarted, requestId, sourceId, sourceId, SafDestinationSourceKind.RISU_SAVE, 1_004))
    assertNull(beginReservedSafCopy(restarted, requestId, jobId, sourceId, SafDestinationSourceKind.DOWNLOAD, 1_004))
    val copying = beginReservedSafCopy(restarted, requestId, jobId, sourceId, SafDestinationSourceKind.RISU_SAVE, 1_004)!!
    store.save(copying)
    assertEquals(jobId, store.load()!!.jobId)
    assertEquals(sourceId, store.load()!!.exportId)
    assertNull(beginReservedSafCopy(copying, requestId, jobId, sourceId, SafDestinationSourceKind.RISU_SAVE, 1_005))
    assertEquals(SafDestinationRecoveryAction.CLEAN_PARTIAL, decideSafDestinationRecovery(copying, false))
  }

  @Test
  fun `reservation cancellation preserves unknown ownership including a late picker result`() {
    val requestId = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val picking = SafDestinationRecord(requestId, requestId, SafDestinationPhase.CANCELLING,
      null, null, null, emptyList(), 1_000, reservedDestination = true, exportBound = false, copyStarted = false)
    assertNull(expiredSafDestinationState(picking, requestId, 1_000 + DESTINATION_PICKER_STALE_MILLIS))
    assertEquals(SafDestinationRecoveryAction.WAIT_FOR_PICKER,
      decideSafDestinationRecovery(picking, true, 1_000 + DESTINATION_PICKER_STALE_MILLIS))
    val selected = selectedSafDestinationState(picking, requestId, "content://synthetic/new", false, 1_001)!!
    assertEquals(SafDestinationPhase.CANCELLING, selected.phase)
    val restored = picking.copy(phase = SafDestinationPhase.PICKING)
    val lateAfterRestart = selectedSafDestinationState(restored, requestId, "content://synthetic/new",
      restoredSafDestinationCancellation(restored), 1_001)!!
    assertEquals(SafDestinationPhase.CANCELLING, lateAfterRestart.phase)
    assertTrue(interruptedSafDestinationWarnings(selected).contains("partial-destination-may-remain"))
    assertTrue(interruptedSafDestinationWarnings(lateAfterRestart).contains("partial-destination-may-remain"))
    val legacy = selected.copy(reservedDestination = false, exportBound = true, copyStarted = true)
    assertTrue(interruptedSafDestinationWarnings(legacy)
      .contains("partial-destination-may-remain"))
  }

  @Test
  fun `released v3 destination defaults preserve nonowned documents and unbound cancellations can acknowledge`() {
    val requestId = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    val storeFile = temporaryDirectory().resolve("destination.json")
    val store = SafDestinationStateStore(storeFile, testAtomicPublisher)
    val legacy = SafDestinationRecord(requestId, requestId, SafDestinationPhase.COPYING,
      "content://synthetic/preexisting", null, null, emptyList(), 1_000)
    store.save(legacy)
    storeFile.writeText(storeFile.readText().replace("\"reservedDestination\":false,", "")
      .replace("\"exportBound\":true,", "").replace("\"copyStarted\":true,", "").replace("\"jobId\":null,", ""))
    assertEquals(legacy, store.load())
    assertTrue(interruptedSafDestinationWarnings(store.load()!!)
      .contains("partial-destination-may-remain"))
    val unbound = legacy.copy(phase = SafDestinationPhase.CANCELLED, destinationUri = null,
      reservedDestination = true, exportBound = false, copyStarted = false)
    store.save(unbound)
    assertTrue(acknowledgeSafDestinationExport(requestId, store::load,
      { error("An unstarted export has no publication prerequisite") },
      { error("An unstarted export has no source to discard") }, store::clear))
    assertNull(store.load())
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
  fun `interrupted selected destination always reports possible partial file`() {
    assertEquals(
      listOf("android-saf-provider-not-atomic", "partial-destination-may-remain"),
      interruptedSafDestinationWarnings(),
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
  fun `cancellation before destination open preserves existing document bytes`() = runBlocking {
    val root = temporaryDirectory()
    val source = root.resolve("synthetic.risudat").apply { writeText("replacement") }
    val destination = root.resolve("existing.risudat").apply { writeText("original") }
    for (cancelDuringSourceOpen in listOf(false, true)) {
      var cancelled = !cancelDuringSourceOpen
      val failure = try {
        copySafDestinationOnIo(source,
          openSource = { source.inputStream().also { cancelled = true } },
          openDestination = { destination.outputStream() },
          deletePartial = { destination.delete() },
          createdDocument = false,
          isCancelled = { cancelled },
        )
        null
      } catch (error: SafDestinationException) { error }
      assertEquals("cancelled", failure?.code)
      assertEquals("original", destination.readText())
    }
  }

  @Test
  fun `failed or cancelled selected document is never deleted`() = runBlocking {
    val root = temporaryDirectory()
    val source = root.resolve("synthetic.risudat")
    source.writeBytes(ByteArray(10) { it.toByte() })
    for (cancelled in listOf(false, true)) {
      var deleteAttempts = 0
      var cancel = false
      val failure = try {
        copySafDestinationOnIo(
          source,
          openDestination = { if (cancelled) collectingOutput(mutableListOf()) else failingOutput(afterBytes = 4) },
          deletePartial = { deleteAttempts += 1; true },
          createdDocument = false,
          isCancelled = { cancel },
          onProgress = { if (cancelled) cancel = true },
          bufferBytes = 4,
        )
        null
      } catch (error: SafDestinationException) {
        error
      }
      assertEquals(0, deleteAttempts)
      assertEquals(if (cancelled) "cancelled" else "destination-write-failed", failure?.code)
      assertEquals(
        listOf("android-saf-provider-not-atomic", "partial-destination-may-remain"),
        failure?.warningCodes,
      )
    }
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
    assertFalse(managedExportHasNoPublicationPrerequisites(requireNotNull(resolveManagedExportSource(appData, source.path))))
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
  fun `destination source accepts only exact app-owned download handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-download-$id.bin").apply {
      writeBytes(byteArrayOf(1, 2, 3))
    }
    val outside = appData.resolve("outside/risu-download-$id.bin").apply {
      parentFile!!.mkdirs()
      writeBytes(byteArrayOf(9))
    }
    val otherSuffix = handoffs.resolve("risu-download-$id.json").apply {
      writeBytes(byteArrayOf(8))
    }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(SafDestinationSourceKind.DOWNLOAD, managedExportSourceKind(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, otherSuffix.path))
  }

  @Test
  fun `managed publication proof survives source cleanup without approving deferred backups`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs").apply { mkdirs() }
    val id = "99999999-9999-4999-8999-999999999999"
    val store = SafDestinationStateStore(appData.resolve("destination.json"), testAtomicPublisher)
    val names = listOf("risunest-rescue-$id.risunest-rescue.zip", "risu-charx-$id.charx",
      "risu-charx-$id.jpeg", "risu-character-card-$id.json", "risu-character-card-$id.png",
      "risu-module-$id.risum", "risu-dataset-$id.json")
    for (name in names) {
      val source = handoffs.resolve(name).apply { writeBytes(byteArrayOf(1)) }
      val owned = requireNotNull(resolveManagedExportSource(appData, source.path))
      val record = terminalDestinationRecord(id, id, SafDestinationSourceKind.RISU_SAVE,
        managedExportHasNoPublicationPrerequisites(owned))
      store.save(record)
      assertTrue(source.delete())
      val restored = requireNotNull(store.load())
      assertTrue(androidSafDestinationScriptForRecord(restored, null)
        .contains("\"publicationPrerequisitesComplete\":true"))
      assertTrue(acknowledgeStoredDestination(appData, store, id))
      assertNull(store.load())
    }
    for (name in listOf("risunest-backup-$id.risunest", "risusave-$id.risudat", "unknown-$id.bin")) {
      assertFalse(managedExportHasNoPublicationPrerequisites(handoffs.resolve(name)))
    }
    val unstarted = terminalDestinationRecord(id, id, SafDestinationSourceKind.SCREENSHOT, true)
      .copy(reservedDestination = true, exportBound = false, copyStarted = false)
    assertTrue(androidSafDestinationScriptForRecord(unstarted, null).contains("\"copyStarted\":false"))
    assertTrue(androidSafDestinationJson(id, state = "cancelled", warningCodes = emptyList())
      .contains("\"copyStarted\":true"))
  }

  @Test
  fun `a download terminal is acknowledged without publication proof and keeps its handoff`() {
    val appData = temporaryDirectory()
    val stateFile = appData.resolve("native-file-jobs/android-saf-destination.json")
    val store = SafDestinationStateStore(stateFile, testAtomicPublisher)
    val download = terminalDestinationRecord(
      requestId = "d0d0d0d0-d0d0-40d0-80d0-d0d0d0d0d0d0",
      exportId = "e0e0e0e0-e0e0-40e0-80e0-e0e0e0e0e0e0",
      sourceKind = SafDestinationSourceKind.DOWNLOAD,
      publicationPrerequisitesComplete = false,
    )
    val handoff = appData.resolve("native-file-jobs/handoffs/risu-download-${download.exportId}.bin")
    handoff.parentFile!!.mkdirs()
    handoff.writeBytes(byteArrayOf(1))
    store.save(download)
    assertEquals(SafDestinationSourceKind.DOWNLOAD, store.load()?.sourceKind)

    assertTrue(acknowledgeStoredDestination(appData, store, download.requestId))
    assertNull(store.load())
    assertTrue(handoff.isFile)

    store.save(download)
    assertTrue(handoff.delete())
    assertTrue(acknowledgeStoredDestination(appData, store, download.requestId))
    assertNull(store.load())
  }

  @Test
  fun `destination source accepts only exact app-owned dataset handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risu-dataset-$id.json").apply { writeBytes(byteArrayOf(1)) }
    val outside = appData.resolve("outside/risu-dataset-$id.json").apply {
      parentFile!!.mkdirs()
      writeBytes(byteArrayOf(2))
    }
    val otherSuffix = handoffs.resolve("risu-dataset-$id.bin").apply { writeBytes(byteArrayOf(3)) }

    assertEquals(source.canonicalFile, resolveManagedExportSource(appData, source.path))
    assertEquals(id, managedExportId(source))
    assertEquals(SafDestinationSourceKind.RISU_SAVE, managedExportSourceKind(source))
    assertEquals(source.canonicalFile, resolveManagedExportById(appData, id))
    assertNull(resolveManagedExportSource(appData, outside.path))
    assertNull(resolveManagedExportSource(appData, otherSuffix.path))
  }

  @Test
  fun `destination source accepts only exact app-owned portable backup handoffs`() {
    val appData = temporaryDirectory()
    val handoffs = appData.resolve("native-file-jobs/handoffs")
    handoffs.mkdirs()
    val id = "99999999-9999-4999-8999-999999999999"
    val source = handoffs.resolve("risunest-backup-$id.risunest")
    source.writeBytes(byteArrayOf(1, 2, 3))
    assertFalse(managedExportHasNoPublicationPrerequisites(requireNotNull(resolveManagedExportSource(appData, source.path))))
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
    assertEquals("backup file.risudat", safeSafDestinationName("backup file"))
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
  fun `a download picker keeps the name and extension the app gave it`() {
    fun download(name: String) = safDestinationPickerName(name, SafDestinationSourceKind.DOWNLOAD)
    assertEquals("chat.txt", download("chat.txt"))
    assertEquals("Alice_chat.html", download("folder/Alice_chat.html"))
    assertEquals("subtitle.vtt", download("subtitle.vtt"))
    assertEquals("translated.po", download("translated.po"))
    assertEquals("preset.risup", download("preset.risup"))
    assertEquals("export", download("export"))
    assertEquals("opened-file", download("///"))
    val long = "c".repeat(200) + ".json"
    assertEquals(180, download(long).length)
    assertTrue(download(long).endsWith(".json"))
    val longText = "c".repeat(200) + ".txt"
    assertEquals(180, download(longText).length)
    assertTrue(download(longText).endsWith(".txt"))
    assertEquals(180, download("c".repeat(200) + ".a‮").length)
    assertEquals(
      "chat.txt.risudat",
      safDestinationPickerName("chat.txt", SafDestinationSourceKind.RISU_SAVE),
    )
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
