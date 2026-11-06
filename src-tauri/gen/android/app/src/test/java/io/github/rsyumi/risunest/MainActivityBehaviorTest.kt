package io.github.rsyumi.risunest

import android.content.ComponentCallbacks2
import android.os.Build
import androidx.core.view.WindowInsetsCompat
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class MainActivityBehaviorTest {
  @Test
  fun `download fallback sends only external web URLs to a browser`() {
    for (url in listOf("https://example.com/file.zip", "http://example.com/file?download=1")) {
      assertEquals(url, externalDownloadUrl(url))
    }
    for (url in listOf("blob:null/file", "data:text/plain,test", "file:///secret", "intent://host", "http://tauri.localhost/file", "http://user@example.com/", "http:///missing-host")) {
      assertEquals(null, externalDownloadUrl(url))
    }
  }

  @Test
  fun `repeated back requests cannot replace a pending exit decision`() {
    val gate = ExitFlushGate()
    assertEquals(true, gate.begin("first"))
    assertEquals(false, gate.begin("second"))
    assertEquals(true, gate.isPending("first"))
    assertEquals(false, gate.shouldFinish("second"))
    gate.cancel("first")
    assertEquals(false, gate.shouldFinish("first"))
    assertEquals(true, gate.begin("third"))
  }

  @get:Rule val temporaryFolder = TemporaryFolder.builder().assureDeletion().build()

  @Test
  fun `content picker recognizes binary cards and upstream metadata formats only`() {
    for (name in listOf("card.CHARX", "card.png", "module.risum", "book.lorebook", "module.json", "card.JPEG")) {
      assertEquals(true, isNativeContentSource(name))
    }
    assertEquals(false, isNativeContentSource("database.risudat"))
    assertEquals(false, isNativeContentSource("program.exe"))
  }

  @Test
  fun `SAF RisuSave import is enabled by default`() {
    assertEquals(true, BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS)
  }

  @Test
  fun `native SAF routing spools only restore and recognized character candidates`() {
    assertEquals(true, shouldUseNativeFileJobSpool("backup.risudat"))
    assertEquals(true, shouldUseNativeFileJobSpool("BACKUP.RISUDAT"))
    assertEquals(true, shouldUseNativeFileJobSpool("backup.risunest"))
    assertEquals(true, shouldUseNativeFileJobSpool("backup.bin"))
    assertEquals(true, shouldUseNativeFileJobSpool("BACKUP.BIN"))
    assertEquals(true, shouldUseNativeFileJobSpool("card.PnG"))
    assertEquals(true, shouldUseNativeFileJobSpool("module.RiSuM"))
    assertEquals(true, shouldUseNativeFileJobSpool("book.LoReBoOk"))
    assertEquals(true, shouldUseNativeFileJobSpool("BACKUP.RISUNEST"))
    assertEquals(false, shouldUseNativeFileJobSpool("backup.risulossless"))
    assertEquals(true, shouldUseNativeFileJobSpool("character.charx"))
    assertEquals(true, shouldUseNativeFileJobSpool("character.json"))
    assertEquals(true, shouldUseNativeFileJobSpool("character.jpg"))
    assertEquals(true, shouldUseNativeFileJobSpool("character.JPEG"))
    assertEquals(false, shouldUseNativeFileJobSpool("module.risup"))
    assertEquals(false, shouldUseNativeFileJobSpool("preset.risup"))
    assertEquals(true, shouldUseNativeFileJobSpool("character.png"))
    assertEquals(false, shouldUseNativeFileJobSpool("unknown"))
  }

  @Test
  fun `long native SAF candidate names preserve their persisted extension for dispatch`() {
    val character = safeSafDisplayName("a".repeat(181) + ".charx")
    val risuSave = safeSafDisplayName("b".repeat(181) + ".risudat")
    val png = safeSafDisplayName("p".repeat(181) + ".PNG")
    val risum = safeSafDisplayName("m".repeat(181) + ".RISUM")

    assertEquals(180, character.length)
    assertEquals(true, character.endsWith(".charx"))
    assertEquals(true, shouldUseNativeFileJobSpool(character))
    assertEquals(180, risuSave.length)
    assertEquals(true, risuSave.endsWith(".risudat"))
    assertEquals(180, png.length)
    assertEquals(true, png.endsWith(".PNG"))
    assertEquals(true, shouldUseNativeFileJobSpool(png))
    assertEquals(180, risum.length)
    assertEquals(true, risum.endsWith(".RISUM"))
    assertEquals(true, shouldUseNativeFileJobSpool(risum))
    assertEquals(true, shouldUseNativeFileJobSpool(risuSave))
    for (suffix in listOf(".risunest", ".RISUDAT", ".BiN")) {
      val backup = safeSafDisplayName("b".repeat(181) + suffix)
      assertEquals(180, backup.length)
      assertEquals(true, backup.endsWith(suffix))
      assertEquals(true, isBackupSource(backup))
      assertEquals(true, shouldUseNativeFileJobSpool(backup))
    }
  }

  @Test
  fun `common backup picker accepts only current backup suffixes`() {
    val accepted = listOf(
      "backup.risunest", "BACKUP.RISUNEST", "backup.risudat", "BACKUP.RISUDAT", "backup.bin", "BACKUP.BIN",
    )
    for (name in accepted) {
      assertEquals(name, true, isBackupSource(name))
    }
    val rejected = listOf(
      "backup.risulossless", "backup.zip", "backup.risunest.txt", "backup.risudat.bin.exe", "backup", "character.charx",
    )
    for (name in rejected) {
      assertEquals(name, false, isBackupSource(name))
    }
  }

  @Test
  fun `ready document delivery dispatches opened files and falls back to its startup queue`() {
    val script = openedFilesEventScript(listOf("/data/cache/opened_files/1-0-preset.risup"))

    assertEquals(true, script.contains("new CustomEvent('risu-opened-files'"))
    assertEquals(true, script.contains("cancelable:true"))
    assertEquals(true, script.contains("\"/data/cache/opened_files/1-0-preset.risup\""))
    assertEquals(true, script.contains("if(window.dispatchEvent(event)){"))
    assertEquals(true, script.contains("window.tauriOpenedFiles="))
    // Delivery never replaces an earlier undrained startup batch.
    assertEquals(false, script.startsWith("window.tauriOpenedFiles="))
  }

  @Test
  fun `ready document delivery escapes opened file paths`() {
    assertEquals(
      true,
      openedFilesEventScript(listOf("C:\\opened\nfile.risup"))
        .contains("\"C:\\\\opened\\u000afile.risup\""),
    )
  }

  @Test
  fun `claiming an open does not consume it before durable preparation`() {
    val claims = OpenedFileClaimRegistry()
    assertEquals(true, claims.claim("source", consumed = false))
    assertEquals(false, claims.claim("source", consumed = false))
    claims.release("source")
    assertEquals(true, claims.claim("source", consumed = false))
    claims.release("source")
    assertEquals(false, claims.claim("source", consumed = true))
  }

  @Test
  fun `restored launch fingerprint identifies the same URI payload without mutable Intent extras`() {
    val original = openedFileIntentFingerprint(
      "android.intent.action.SEND_MULTIPLE",
      listOf("content://provider/a", "content://provider/b"),
    )

    assertEquals(
      original,
      openedFileIntentFingerprint(
        "android.intent.action.SEND_MULTIPLE",
        listOf("content://provider/a", "content://provider/b"),
      ),
    )
    assertEquals(
      false,
      original == openedFileIntentFingerprint(
        "android.intent.action.SEND_MULTIPLE",
        listOf("content://provider/b", "content://provider/a"),
      ),
    )
  }

  @Test
  fun `web view provider must meet the Vite 8 Chrome 111 baseline`() {
    assertEquals(
      WebViewProviderStatus.SUPPORTED,
      decideWebViewProvider("com.google.android.webview", "111.0.5563.116").status,
    )
    assertEquals(
      WebViewProviderStatus.OUTDATED,
      decideWebViewProvider("com.google.android.webview", "110.0.5481.154").status,
    )
  }

  @Test
  fun `missing and unreadable web view providers are diagnosed separately`() {
    assertEquals(
      WebViewProviderStatus.MISSING,
      decideWebViewProvider(null, null).status,
    )
    assertEquals(
      WebViewProviderStatus.UNKNOWN_VERSION,
      decideWebViewProvider("com.google.android.webview", "not-a-version").status,
    )
  }

  @Test
  fun `renderer recovery marker is consumed only once`() {
    var marked = false
    val marker = OneShotRecoveryMarker(
      isMarked = { marked },
      setMarked = { marked = it },
    )

    marker.mark()

    assertEquals(true, marker.consume())
    assertEquals(false, marker.consume())
  }

  @Test
  fun `renderer recovery cleans the dead view before restarting`() {
    val operations = mutableListOf<String>()
    val coordinator = RendererRecoveryCoordinator { _, _ -> }

    assertEquals(
      true,
      coordinator.recover(
        removeFromParent = { operations.add("remove-parent") },
        removeJavascriptBridge = { operations.add("remove-bridge") },
        destroyView = { operations.add("destroy-view") },
        clearReference = { operations.add("clear-reference") },
        markRecovery = { operations.add("mark-recovery") },
        restart = {
          operations.add("restart")
          true
        },
      ),
    )

    assertEquals(
      listOf(
        "remove-parent",
        "remove-bridge",
        "destroy-view",
        "clear-reference",
        "mark-recovery",
        "restart",
      ),
      operations,
    )
  }

  @Test
  fun `renderer recovery logs failures and declines handling when restart fails`() {
    val operations = mutableListOf<String>()
    val failures = mutableListOf<String>()
    val coordinator = RendererRecoveryCoordinator { step, error ->
      failures.add("$step: ${error.message}")
    }

    assertEquals(
      false,
      coordinator.recover(
        removeFromParent = {
          operations.add("remove-parent")
          error("remove failed")
        },
        removeJavascriptBridge = { operations.add("remove-bridge") },
        destroyView = {
          operations.add("destroy-view")
          error("destroy failed")
        },
        clearReference = { operations.add("clear-reference") },
        markRecovery = {
          operations.add("mark-recovery")
          error("marker failed")
        },
        restart = {
          operations.add("restart")
          error("restart failed")
        },
      ),
    )

    assertEquals(
      listOf(
        "remove-parent",
        "remove-bridge",
        "destroy-view",
        "clear-reference",
        "mark-recovery",
        "restart",
      ),
      operations,
    )
    assertEquals(
      listOf(
        "remove-from-parent: remove failed",
        "destroy-view: destroy failed",
        "mark-recovery: marker failed",
        "restart: restart failed",
      ),
      failures,
    )
  }

  @Test
  fun `renderer recovery propagates a returning restart fallback failure`() {
    val coordinator = RendererRecoveryCoordinator { _, _ -> }

    assertEquals(
      false,
      coordinator.recover(
        removeFromParent = {},
        removeJavascriptBridge = {},
        destroyView = {},
        clearReference = {},
        markRecovery = {},
        restart = { false },
      ),
    )
  }

  @Test
  fun `duplicate renderer termination callbacks recover only once`() {
    var recoveries = 0
    val coordinator = RendererRecoveryCoordinator { _, _ -> }
    val recover = {
      coordinator.recover(
        removeFromParent = { recoveries += 1 },
        removeJavascriptBridge = {},
        destroyView = {},
        clearReference = {},
        markRecovery = {},
        restart = { true },
      )
    }

    assertEquals(true, recover())
    assertEquals(true, recover())
    assertEquals(1, recoveries)
  }

  @Test
  fun `cold restart relaunches the task before terminating the process`() {
    val operations = mutableListOf<String>()
    val dispatcher = ColdRestartDispatcher(
      relaunchTask = { operations.add("relaunch-task") },
      terminateProcess = { operations.add("terminate-process") },
      logFailure = { _, _ -> },
    )

    assertEquals(false, dispatcher.restart())

    assertEquals(listOf("relaunch-task", "terminate-process"), operations)
  }

  @Test
  fun `cold restart still attempts process termination when relaunch throws`() {
    val operations = mutableListOf<String>()
    val failures = mutableListOf<String>()
    val dispatcher = ColdRestartDispatcher(
      relaunchTask = {
        operations.add("relaunch-task")
        error("launch failed")
      },
      terminateProcess = {
        operations.add("terminate-process")
        error("termination failed")
      },
      logFailure = { step, error -> failures.add("$step: ${error.message}") },
    )

    assertEquals(false, dispatcher.restart())

    assertEquals(listOf("relaunch-task", "terminate-process"), operations)
    assertEquals(
      listOf(
        "relaunch-task: launch failed",
        "terminate-process: termination failed",
      ),
      failures,
    )
  }

  @Test
  fun `activity start and stop reach the page`() {
    val events = mutableListOf<Pair<String, Int?>>()
    val dispatcher = LifecycleFlushDispatcher { reason, trimLevel -> events.add(reason to trimLevel) }

    dispatcher.onStop()
    dispatcher.onStart()

    assertEquals(listOf("stop" to null, "start" to null), events)
  }

  @Test
  fun `trim memory below UI hidden does not request a lifecycle flush`() {
    val events = mutableListOf<Pair<String, Int?>>()
    val dispatcher = LifecycleFlushDispatcher { reason, trimLevel -> events.add(reason to trimLevel) }

    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN - 1)

    assertEquals(emptyList<Pair<String, Int?>>(), events)
  }

  @Test
  fun `trim memory at or above UI hidden requests a lifecycle flush with its level`() {
    val events = mutableListOf<Pair<String, Int?>>()
    val dispatcher = LifecycleFlushDispatcher { reason, trimLevel -> events.add(reason to trimLevel) }

    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN)
    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN + 1)

    assertEquals(
      listOf(
        "trim-memory" to ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN,
        "trim-memory" to ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN + 1,
      ),
      events,
    )
  }

  @Test
  fun `system bars and display cutout remain outside the web view`() {
    val margins = resolveWebViewMargins(
      systemBars = WebViewMargins(left = 0, top = 24, right = 0, bottom = 48),
      displayCutout = WebViewMargins(left = 8, top = 32, right = 8, bottom = 0),
    )

    assertEquals(WebViewMargins(left = 8, top = 32, right = 8, bottom = 48), margins)
  }

  @Test
  fun `keyboard inset remains available to the web view`() {
    assertEquals(0, nativeMarginInsetTypes() and WindowInsetsCompat.Type.ime())
  }

  @Test
  fun `keyboard shrinks the web view only before Android 11`() {
    val margins = WebViewMargins(left = 0, top = 24, right = 0, bottom = 126)

    assertEquals(126, resolveWebViewBottomMargin(Build.VERSION_CODES.P, margins, imeBottom = 0))
    assertEquals(904, resolveWebViewBottomMargin(Build.VERSION_CODES.P, margins, imeBottom = 904))
    assertEquals(904, resolveWebViewBottomMargin(Build.VERSION_CODES.Q, margins, imeBottom = 904))
    assertEquals(126, resolveWebViewBottomMargin(Build.VERSION_CODES.R, margins, imeBottom = 904))
    assertEquals(126, resolveWebViewBottomMargin(36, margins, imeBottom = 904))
  }

  @Test
  fun `root back is offered to the page as a cancelable event`() {
    val script = rootBackScript()

    assertTrue(script.contains("new Event('risunest-root-back',{cancelable:true})"))
    assertTrue(script.contains("window.dispatchEvent(e);return e.defaultPrevented"))
    assertTrue(pageUsedRootBack("true"))
    assertFalse(pageUsedRootBack("false"))
    assertFalse(pageUsedRootBack("null"))
    assertFalse(pageUsedRootBack(null))
  }

  @Test
  fun `opened file names keep only a safe leaf name`() {
    assertEquals("a.charx", sanitizeOpenedFileName("a.charx"))
    assertEquals("b.risum", sanitizeOpenedFileName("primary:Download/b.risum"))
    assertEquals("c_d.risup", sanitizeOpenedFileName("c d.risup"))
    assertEquals("opened-file", sanitizeOpenedFileName("///"))
  }

  @Test
  fun `opened file spool script exposes the cancellable request and tokens instead of paths`() {
    val script = androidSpoolBatchScript(
      requestId = "request-1",
      batch = SafSpoolBatch(
        ready = listOf(
          SafSpoolReady(
            token = "11111111-1111-4111-8111-111111111111",
            displayName = "a\"b\\c\nd.risudat",
            bytes = 9,
            totalBytes = null,
          ),
        ),
        failures = emptyList(),
      ),
    )

    assertEquals(true, script.contains("window.tauriOpenedFileSpools="))
    assertEquals(true, script.contains("window.tauriOpenedFileSpools?.ready"))
    assertEquals(true, script.contains("window.tauriOpenedFileSpools?.failures"))
    assertEquals(true, script.contains("\"requestId\":\"request-1\""))
    assertEquals(true, script.contains("11111111-1111-4111-8111-111111111111"))
    assertEquals(true, script.contains("a\\\"b\\\\c\\nd.risudat"))
    assertEquals(false, script.contains("/data/opened"))
  }

  @Test
  fun `backup source picker uses its dedicated event without changing opened file state`() {
    val script = androidBackupSourcePickedScript(
      requestId = "11111111-1111-4111-8111-111111111111",
      batch = SafSpoolBatch(
        ready = listOf(
          SafSpoolReady(
            token = "22222222-2222-4222-8222-222222222222",
            displayName = "backup.risunest",
            bytes = 9,
            totalBytes = 9,
          ),
        ),
        failures = emptyList(),
      ),
    )

    assertEquals(true, script.contains("risu-android-backup-source-picked"))
    assertEquals(true, script.contains("\"requestId\":\"11111111-1111-4111-8111-111111111111\""))
    assertEquals(true, script.contains("backup.risunest"))
    assertEquals(false, script.contains("tauriOpenedFileSpools"))
    assertEquals(false, script.contains("risu-android-spool-ready"))
  }

  @Test
  fun `backup source replay retains the general opened file spool event`() {
    val script = androidSpoolBatchScript(
      requestId = "11111111-1111-4111-8111-111111111111",
      batch = SafSpoolBatch(
        ready = listOf(
          SafSpoolReady(
            token = "22222222-2222-4222-8222-222222222222",
            displayName = "backup.risunest",
            bytes = 9,
            totalBytes = 9,
          ),
        ),
        failures = emptyList(),
      ),
    )

    assertEquals(true, script.contains("risu-android-spool-ready"))
    assertEquals(false, script.contains("risu-android-backup-source-picked"))
  }

  @Test
  fun `restored backup picker result uses the general replayable spool event`() {
    val batch = SafSpoolBatch(
      ready = listOf(
        SafSpoolReady(
          token = "22222222-2222-4222-8222-222222222222",
          displayName = "backup.risunest",
          bytes = 9,
          totalBytes = 9,
        ),
      ),
      failures = emptyList(),
    )

    val restored = androidBackupSourceResultScript(
      "11111111-1111-4111-8111-111111111111",
      batch,
      restored = true,
    )
    val live = androidBackupSourceResultScript(
      "11111111-1111-4111-8111-111111111111",
      batch,
      restored = false,
    )

    assertEquals(true, restored.contains("risu-android-spool-ready"))
    assertEquals(true, restored.contains("tauriOpenedFileSpools"))
    assertEquals(false, restored.contains("risu-android-backup-source-picked"))
    assertEquals(true, live.contains("risu-android-backup-source-picked"))
    assertEquals(false, live.contains("risu-android-spool-ready"))
  }

  @Test
  fun `restored legacy backup picker result uses the general replayable spool event`() {
    val batch = SafSpoolBatch(
      ready = listOf(
        SafSpoolReady(
          token = "22222222-2222-4222-8222-222222222222",
          displayName = "backup.bin",
          bytes = 9,
          totalBytes = 9,
        ),
      ),
      failures = emptyList(),
    )

    val restored = androidLegacyBackupSourceResultScript(
      "11111111-1111-4111-8111-111111111111",
      batch,
      restored = true,
    )
    val live = androidLegacyBackupSourceResultScript(
      "11111111-1111-4111-8111-111111111111",
      batch,
      restored = false,
    )

    assertEquals(true, restored.contains("risu-android-spool-ready"))
    assertEquals(true, restored.contains("tauriOpenedFileSpools"))
    assertEquals(false, restored.contains("risu-android-legacy-backup-source-picked"))
    assertEquals(true, live.contains("risu-android-legacy-backup-source-picked"))
    assertEquals(false, live.contains("risu-android-spool-ready"))
  }

  @Test
  fun `SAF progress script uses one bounded event shape for source and destination`() {
    val source = androidSafProgressScript(
      requestId = "source-1",
      operation = "source-copy",
      copiedBytes = 64,
      totalBytes = null,
      token = "11111111-1111-4111-8111-111111111111",
    )
    val destination = androidSafProgressScript(
      requestId = "destination-1",
      operation = "destination-copy",
      copiedBytes = 128,
      totalBytes = 256,
      token = null,
    )

    assertEquals(true, source.contains("risu-android-saf-progress"))
    assertEquals(true, source.contains("\"requestId\":\"source-1\""))
    assertEquals(true, source.contains("\"operation\":\"source-copy\""))
    assertEquals(true, source.contains("\"totalBytes\":null"))
    assertEquals(true, destination.contains("\"operation\":\"destination-copy\""))
    assertEquals(true, destination.contains("\"totalBytes\":256"))
    assertEquals(true, destination.contains("\"token\":null"))
  }

  @Test
  fun `destination record replay preserves publication readiness proof`() {
    val script = androidSafDestinationScriptForRecord(
      SafDestinationRecord(
        requestId = "11111111-1111-4111-8111-111111111111",
        exportId = "22222222-2222-4222-8222-222222222222",
        phase = SafDestinationPhase.SUCCEEDED,
        destinationUri = "content://provider/document/42",
        bytes = 42,
        code = null,
        warningCodes = emptyList(),
        updatedAtMillis = 2_000,
        publicationPrerequisitesComplete = true,
      ),
      message = null,
    )

    assertEquals(true, script.contains("\"publicationPrerequisitesComplete\":true"))
  }

  @Test
  fun `SAF progress throttle measures from the last dispatched event`() {
    val throttle = SafProgressThrottle(intervalMillis = 100)

    assertEquals(true, throttle.shouldDispatch("request:token", 1_000))
    assertEquals(false, throttle.shouldDispatch("request:token", 1_050))
    assertEquals(true, throttle.shouldDispatch("request:token", 1_100))
    assertEquals(false, throttle.shouldDispatch("request:token", 1_150))
    assertEquals(true, throttle.shouldDispatch("request:token", 1_200))
  }

  @Test
  fun `clearing SAF progress throttle releases every token for one request`() {
    val throttle = SafProgressThrottle(intervalMillis = 100)
    assertEquals(true, throttle.shouldDispatch("first:a", 1_000))
    assertEquals(true, throttle.shouldDispatch("first:b", 1_000))
    assertEquals(true, throttle.shouldDispatch("second:a", 1_000))

    throttle.clear("first")

    assertEquals(true, throttle.shouldDispatch("first:a", 1_001))
    assertEquals(true, throttle.shouldDispatch("first:b", 1_001))
    assertEquals(false, throttle.shouldDispatch("second:a", 1_001))
  }

  @Test
  fun `SAF picker launch failure runs cleanup instead of escaping`() {
    val events = mutableListOf<String>()

    launchSafSourcePicker(
      launch = { throw IllegalStateException("no picker") },
      onFailure = { events.add("failed") },
    )

    assertEquals(listOf("failed"), events)
  }

  @Test
  fun `legacy opened file cleanup removes only stale regular files`() {
    val directory = temporaryFolder.newFolder()
    val stale = directory.resolve("stale.risup").apply {
      writeBytes(byteArrayOf(1))
      setLastModified(1_000)
    }
    val recent = directory.resolve("recent.risup").apply {
      writeBytes(byteArrayOf(2))
      setLastModified(1_950)
    }
    val nested = directory.resolve("nested").apply {
      mkdirs()
      setLastModified(1_000)
    }

    assertEquals(
      listOf("stale.risup"),
      cleanupLegacyOpenedFiles(directory, nowMillis = 2_000, staleAfterMillis = 100),
    )
    assertEquals(false, stale.exists())
    assertEquals(true, recent.exists())
    assertEquals(true, nested.exists())
  }

  @Test
  fun `legacy cleanup retains unacknowledged sources and their durable markers`() {
    val directory = temporaryFolder.newFolder("pending-legacy")
    val source = java.io.File(directory, "source.risup").apply { writeText("synthetic"); setLastModified(1_000) }
    val marker = java.io.File(directory, "source.risup.pending").apply { writeText(""); setLastModified(1_000) }
    val delivered = java.io.File(directory, "source.risup.delivered").apply { writeText(""); setLastModified(1_000) }
    assertEquals(emptyList<String>(), cleanupLegacyOpenedFiles(directory, nowMillis = 2_000, staleAfterMillis = 100))
    assertEquals(true, source.exists())
    assertEquals(true, marker.exists())
    assertEquals(true, delivered.exists())
    marker.delete()
    assertEquals(
      listOf("source.risup", "source.risup.delivered"),
      cleanupLegacyOpenedFiles(directory, nowMillis = 2_000, staleAfterMillis = 100),
    )
  }

  @Test
  fun `legacy replay removes and reports a file an earlier WebView received`() {
    val directory = temporaryFolder.newFolder("replay-legacy")
    val prefix = "a".repeat(64)
    fun pending(name: String, delivered: Boolean): java.io.File {
      val source = java.io.File(directory, name).apply { writeText("synthetic") }
      java.io.File(directory, "$name.pending").writeText("")
      if (delivered) java.io.File(directory, "$name.delivered").writeText("")
      return source
    }
    val waiting = pending("$prefix-0-waiting.risup", delivered = false)
    val interrupted = pending("$prefix-1-large preset.risup", delivered = true)
    val current = pending("$prefix-2-current.risup", delivered = true)
    java.io.File(directory, "orphan.risup.pending").writeText("")

    val replay = replayLegacyOpenedFiles(directory, setOf(current.absolutePath))

    assertEquals(listOf(waiting.absolutePath), replay.deliver)
    assertEquals(listOf("large preset.risup"), replay.interrupted)
    assertEquals(
      listOf(
        "$prefix-0-waiting.risup",
        "$prefix-0-waiting.risup.pending",
        "$prefix-2-current.risup",
        "$prefix-2-current.risup.delivered",
        "$prefix-2-current.risup.pending",
        "orphan.risup.pending",
      ),
      directory.list().orEmpty().sorted(),
    )
    assertEquals(false, interrupted.exists())
    val again = replayLegacyOpenedFiles(directory, emptySet())
    assertEquals(listOf(waiting.absolutePath), again.deliver)
    assertEquals(listOf("current.risup"), again.interrupted)
  }

  @Test
  fun `legacy backup picker result uses a dedicated token-only event`() {
    val script = androidLegacyBackupSourcePickedScript(
      requestId = "11111111-1111-4111-8111-111111111111",
      batch = SafSpoolBatch(
        ready = listOf(
          SafSpoolReady(
            token = "22222222-2222-4222-8222-222222222222",
            displayName = "backup.bin",
            bytes = 4_294_967_296,
            totalBytes = 4_294_967_296,
          ),
        ),
        failures = emptyList(),
      ),
    )

    assertEquals(true, script.contains("risu-android-legacy-backup-source-picked"))
    assertEquals(true, script.contains("backup.bin"))
    assertEquals(false, script.contains("tauriOpenedFileSpools"))
    assertEquals(false, script.contains("risu-android-spool-ready"))
    assertEquals(false, script.contains("Uint8Array"))
  }

  @Test
  fun `exit flush finishes once per token`() {
    val gate = ExitFlushGate()
    gate.begin("exit-1")

    assertEquals(true, gate.shouldFinish("exit-1"))
    assertEquals(false, gate.shouldFinish("exit-1"))
  }

  @Test
  fun `stale exit flush tokens do not finish the activity`() {
    val gate = ExitFlushGate()
    gate.begin("exit-2")

    assertEquals(false, gate.shouldFinish("exit-1"))
    assertEquals(true, gate.shouldFinish("exit-2"))
  }

  @Test
  fun `a held exit flush token does not finish the activity`() {
    val gate = ExitFlushGate()
    gate.begin("exit-1")
    gate.cancel("exit-1")

    assertEquals(false, gate.shouldFinish("exit-1"))
  }

  @Test
  fun `cancel ignores stale exit flush tokens`() {
    val gate = ExitFlushGate()
    gate.begin("exit-2")
    gate.cancel("exit-1")

    assertEquals(true, gate.shouldFinish("exit-2"))
  }

  @Test
  fun `busy SAF source pick rejection reaches the WebView only through the main thread poster`() {
    val posted = mutableListOf<() -> Unit>()
    val flow = SafSourcePickFlow(posted::add)
    val events = mutableListOf<String>()

    flow.begin(
      acquireSlot = { false },
      registerRequest = { error("a busy slot must not register the request") },
      releaseSlot = { events.add("release") },
      dispatchBusy = { events.add("busy") },
      startPicker = { events.add("picker") },
    )

    // Nothing WebView-bound may run synchronously on the JavaBridge thread.
    assertEquals(emptyList<String>(), events)
    assertEquals(1, posted.size)
    posted.forEach { it() }
    assertEquals(listOf("busy"), events)
  }

  @Test
  fun `duplicate SAF source pick releases the slot and defers the busy dispatch to main`() {
    val posted = mutableListOf<() -> Unit>()
    val flow = SafSourcePickFlow(posted::add)
    val events = mutableListOf<String>()

    flow.begin(
      acquireSlot = { true },
      registerRequest = { false },
      releaseSlot = { events.add("release") },
      dispatchBusy = { events.add("busy") },
      startPicker = { events.add("picker") },
    )

    assertEquals(listOf("release"), events)
    posted.forEach { it() }
    assertEquals(listOf("release", "busy"), events)
  }

  @Test
  fun `admitted SAF source pick launches the picker only through the main thread poster`() {
    val posted = mutableListOf<() -> Unit>()
    val flow = SafSourcePickFlow(posted::add)
    val events = mutableListOf<String>()

    flow.begin(
      acquireSlot = { true },
      registerRequest = { true },
      releaseSlot = { events.add("release") },
      dispatchBusy = { events.add("busy") },
      startPicker = { events.add("picker") },
    )

    assertEquals(emptyList<String>(), events)
    posted.forEach { it() }
    assertEquals(listOf("picker"), events)
  }

  @Test
  fun `notification permission request is skipped below Android 13 and when already granted`() {
    val belowTiramisu = PostNotificationsRequestGate(32, AtomicBoolean(false))
    assertEquals(false, belowTiramisu.shouldRequest { false })

    val granted = PostNotificationsRequestGate(33, AtomicBoolean(false))
    assertEquals(false, granted.shouldRequest { true })
    // A grant must not consume the once-per-process request budget.
    assertEquals(true, granted.shouldRequest { false })
  }

  @Test
  fun `notification permission is requested once per process through the main thread poster`() {
    val posted = mutableListOf<() -> Unit>()
    var launches = 0
    val gate = PostNotificationsRequestGate(34, AtomicBoolean(false))

    repeat(3) {
      requestPostNotificationsIfNeeded(
        gate = gate,
        isGranted = { false },
        postToMain = posted::add,
        launchRequest = { launches += 1 },
      )
    }

    assertEquals(0, launches)
    assertEquals(1, posted.size)
    posted.forEach { it() }
    assertEquals(1, launches)
  }

  @Test
  fun `activity teardown removes the page bridge before stopping background work`() {
    val events = mutableListOf<String>()

    teardownBackgroundWork(
      removeJavascriptBridge = { events.add("remove-bridge") },
      stopBackgroundWork = { events.add("stop") },
    )

    assertEquals(listOf("remove-bridge", "stop"), events)
  }

  @Test
  fun `activity teardown still stops background work when bridge removal fails`() {
    var stopped = false

    assertThrows(IllegalStateException::class.java) {
      teardownBackgroundWork(
        removeJavascriptBridge = { error("bridge failure") },
        stopBackgroundWork = { stopped = true },
      )
    }

    assertEquals(true, stopped)
  }
}
