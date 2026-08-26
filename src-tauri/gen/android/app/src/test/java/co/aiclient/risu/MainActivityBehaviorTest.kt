package co.aiclient.risu

import android.content.ComponentCallbacks2
import androidx.core.view.WindowInsetsCompat
import org.junit.Assert.assertEquals
import org.junit.Test

class MainActivityBehaviorTest {
  @Test
  fun `SAF file jobs stay disabled until physical lifecycle validation`() {
    assertEquals(false, BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS)
  }

  @Test
  fun `disabled SAF jobs retain the legacy tauri opened files contract`() {
    assertEquals(
      "window.tauriOpenedFiles=[\"C:\\\\opened\\u000afile.risudat\"];",
      openedFilesScript(listOf("C:\\opened\nfile.risudat")),
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
    assertEquals(true, script.contains("\"requestId\":\"request-1\""))
    assertEquals(true, script.contains("11111111-1111-4111-8111-111111111111"))
    assertEquals(true, script.contains("a\\\"b\\\\c\\nd.risudat"))
    assertEquals(false, script.contains("/data/opened"))
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
}
