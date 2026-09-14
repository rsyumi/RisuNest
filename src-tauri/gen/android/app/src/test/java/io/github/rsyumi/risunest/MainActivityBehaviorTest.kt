package io.github.rsyumi.risunest

import android.content.ComponentCallbacks2
import androidx.core.view.WindowInsetsCompat
import java.nio.file.Files
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class MainActivityBehaviorTest {
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
  fun `disabled SAF jobs retain the legacy tauri opened files contract`() {
    assertEquals(
      "window.tauriOpenedFiles=[\"C:\\\\opened\\u000afile.risudat\"];",
      openedFilesScript(listOf("C:\\opened\nfile.risudat")),
    )
  }

  @Test
  fun `legacy opened files are injected on a cold start and dispatched on a warm start`() {
    assertEquals(
      LegacyOpenedFileDelivery.DOCUMENT_START_INJECTION,
      legacyOpenedFileDelivery(coldStart = true),
    )
    assertEquals(
      LegacyOpenedFileDelivery.RUNTIME_EVENT,
      legacyOpenedFileDelivery(coldStart = false),
    )
  }

  @Test
  fun `warm start delivery dispatches the opened files and falls back to the startup queue`() {
    val script = openedFilesEventScript(listOf("/data/cache/opened_files/1-0-preset.risup"))

    assertEquals(true, script.contains("new CustomEvent('risu-opened-files'"))
    assertEquals(true, script.contains("cancelable:true"))
    assertEquals(true, script.contains("\"/data/cache/opened_files/1-0-preset.risup\""))
    assertEquals(true, script.contains("if(window.dispatchEvent(event)){"))
    assertEquals(true, script.contains("window.tauriOpenedFiles="))
    // The cold start contract stays a plain assignment, the warm start one never replaces it.
    assertEquals(false, script.startsWith("window.tauriOpenedFiles="))
  }

  @Test
  fun `warm start delivery escapes opened file paths the same way the cold start does`() {
    assertEquals(
      true,
      openedFilesEventScript(listOf("C:\\opened\nfile.risup"))
        .contains("\"C:\\\\opened\\u000afile.risup\""),
    )
  }

  @Test
  fun `restored intent payload is consumed only once before asynchronous work`() {
    var consumed = false
    val marker = RestoredIntentConsumptionMarker(
      isConsumed = { consumed },
      markConsumed = { consumed = true },
    )

    assertEquals(true, marker.claim())
    assertEquals(false, marker.claim())
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
  fun `activity stop requests a lifecycle flush`() {
    val reasons = mutableListOf<String>()
    val dispatcher = LifecycleFlushDispatcher(reasons::add)

    dispatcher.onStop()

    assertEquals(listOf("stop"), reasons)
  }

  @Test
  fun `trim memory below UI hidden does not request a lifecycle flush`() {
    val reasons = mutableListOf<String>()
    val dispatcher = LifecycleFlushDispatcher(reasons::add)

    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN - 1)

    assertEquals(emptyList<String>(), reasons)
  }

  @Test
  fun `trim memory at UI hidden requests a lifecycle flush`() {
    val reasons = mutableListOf<String>()
    val dispatcher = LifecycleFlushDispatcher(reasons::add)

    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN)

    assertEquals(listOf("trim-memory"), reasons)
  }

  @Test
  fun `trim memory above UI hidden requests a lifecycle flush`() {
    val reasons = mutableListOf<String>()
    val dispatcher = LifecycleFlushDispatcher(reasons::add)

    dispatcher.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN + 1)

    assertEquals(listOf("trim-memory"), reasons)
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
    val directory = Files.createTempDirectory("risu-opened-files").toFile()
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
  fun `generation keep alive requests notification permission before checking availability`() {
    val events = mutableListOf<String>()

    val started = beginGenerationKeepAlive(
      requestNotifications = { events.add("request") },
      notificationsEnabled = { events.add("enabled"); false },
      startService = { events.add("start"); true },
    )

    assertEquals(false, started)
    assertEquals(listOf("request", "enabled"), events)
  }

  @Test
  fun `activity teardown removes the generation bridge before stopping generation only`() {
    val events = mutableListOf<String>()
    val owner = GenerationKeepAliveOwner()

    owner.teardown(
      removeJavascriptBridge = { events.add("remove-generation-bridge") },
      stopGeneration = { events.add("stop-generation") },
    )

    assertEquals(listOf("remove-generation-bridge", "stop-generation"), events)
  }

  @Test
  fun `activity teardown still stops generation when bridge removal fails`() {
    var stopped = false
    val owner = GenerationKeepAliveOwner()

    assertThrows(IllegalStateException::class.java) {
      owner.teardown(
        removeJavascriptBridge = { error("bridge failure") },
        stopGeneration = { stopped = true },
      )
    }

    assertEquals(true, stopped)
  }

  @Test
  fun `activity teardown rejects a late bridge begin after ownership closes`() {
    var starts = 0
    var stops = 0
    val owner = GenerationKeepAliveOwner()

    owner.teardown(
      removeJavascriptBridge = {},
      stopGeneration = { stops += 1 },
    )

    assertEquals(false, owner.begin { starts += 1; true })
    owner.teardown(
      removeJavascriptBridge = {},
      stopGeneration = { stops += 1 },
    )
    assertEquals(0, starts)
    assertEquals(1, stops)
  }

  @Test
  fun `activity teardown waits for an admitted bridge begin before stopping`() {
    val events = java.util.Collections.synchronizedList(mutableListOf<String>())
    val startEntered = CountDownLatch(1)
    val teardownAttempted = CountDownLatch(1)
    val releaseStart = CountDownLatch(1)
    val owner = GenerationKeepAliveOwner()

    val beginThread = thread(isDaemon = true) {
      owner.begin {
        events.add("start-enter")
        startEntered.countDown()
        assertEquals(true, releaseStart.await(5, TimeUnit.SECONDS))
        events.add("start-exit")
        true
      }
    }
    assertEquals(true, startEntered.await(5, TimeUnit.SECONDS))
    val teardownThread = thread(isDaemon = true) {
      teardownAttempted.countDown()
      owner.teardown(
        removeJavascriptBridge = { events.add("remove-bridge") },
        stopGeneration = { events.add("stop") },
      )
    }
    assertEquals(true, teardownAttempted.await(5, TimeUnit.SECONDS))
    val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
    while (
      teardownThread.state != Thread.State.BLOCKED &&
      !events.contains("remove-bridge") &&
      System.nanoTime() < deadline
    ) {
      Thread.yield()
    }
    assertEquals(Thread.State.BLOCKED, teardownThread.state)

    releaseStart.countDown()
    beginThread.join(5_000)
    teardownThread.join(5_000)

    assertEquals(false, beginThread.isAlive)
    assertEquals(false, teardownThread.isAlive)
    assertEquals(listOf("start-enter", "start-exit", "remove-bridge", "stop"), events)
  }

  @Test
  fun `late end from a destroyed owner cannot stop a recreated owner generation`() {
    val lifecycle = GenerationForegroundLifecycle()
    val oldOwner = GenerationKeepAliveOwner()
    val freshOwner = GenerationKeepAliveOwner()
    var freshToken = -1L
    var stops = 0

    oldOwner.teardown({}, {})
    assertEquals(true, freshOwner.begin {
      lifecycle.begin { token -> freshToken = token; true }
    })
    assertEquals(true, lifecycle.activate(freshToken, 200, {}, {}))

    assertEquals(false, oldOwner.end { lifecycle.end { stops += 1; true } })
    assertEquals(true, freshOwner.end { lifecycle.end { stops += 1; true } })
    assertEquals(1, stops)
  }
}
