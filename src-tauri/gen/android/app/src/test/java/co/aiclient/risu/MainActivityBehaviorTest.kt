package co.aiclient.risu

import android.content.ComponentCallbacks2
import androidx.core.view.WindowInsetsCompat
import org.junit.Assert.assertEquals
import org.junit.Test

class MainActivityBehaviorTest {
  @Test
  fun `cold restart relaunches the task before terminating the process`() {
    val operations = mutableListOf<String>()
    val dispatcher = ColdRestartDispatcher(
      relaunchTask = { operations.add("relaunch-task") },
      terminateProcess = { operations.add("terminate-process") },
    )

    dispatcher.restart()

    assertEquals(listOf("relaunch-task", "terminate-process"), operations)
  }

  @Test
  fun `cold restart does not terminate the process itself when relaunch throws`() {
    val operations = mutableListOf<String>()
    val dispatcher = ColdRestartDispatcher(
      relaunchTask = {
        operations.add("relaunch-task")
        error("launch failed")
      },
      terminateProcess = { operations.add("terminate-process") },
    )

    try {
      dispatcher.restart()
    } catch (_: IllegalStateException) {
    }

    assertEquals(listOf("relaunch-task"), operations)
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
  fun `opened files script escapes JS string hazards`() {
    val script = openedFilesScript(listOf("/data/opened/a\"b\\c\nd.charx"))

    assertEquals(
      "window.tauriOpenedFiles=[\"/data/opened/a\\\"b\\\\c\\u000ad.charx\"];",
      script,
    )
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
