package co.aiclient.risu

import androidx.core.view.WindowInsetsCompat
import org.junit.Assert.assertEquals
import org.junit.Test

class MainActivityBehaviorTest {
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
  fun `application exit removes the task before terminating the process`() {
    val exitSteps = mutableListOf<String>()

    exitApplication(
      removeTask = { exitSteps += "remove task" },
      terminateProcess = { exitSteps += "terminate process" },
    )

    assertEquals(listOf("remove task", "terminate process"), exitSteps)
  }
}
