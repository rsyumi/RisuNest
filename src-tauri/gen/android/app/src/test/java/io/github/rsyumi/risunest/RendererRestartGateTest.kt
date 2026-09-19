package io.github.rsyumi.risunest

import android.webkit.RenderProcessGoneDetail
import android.webkit.WebView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RendererRestartGateTest {
  @Test fun hiddenRendererWaitsForResumeAndRestartsOnce() {
    val queue = mutableListOf<() -> Unit>()
    var restarts = 0
    val gate = RendererRestartGate({ queue.add(it) }, { restarts++ })
    assertTrue(gate.request())
    assertTrue(gate.request())
    assertTrue(queue.isEmpty())
    gate.onResume()
    gate.onResume()
    assertEquals(1, queue.size)
    queue.removeAt(0)()
    assertEquals(1, restarts)
    gate.onResume()
    assertTrue(queue.isEmpty())
  }

  @Test fun pauseBeforeThePostedRestartKeepsRecoveryPending() {
    val queue = mutableListOf<() -> Unit>()
    var restarts = 0
    val gate = RendererRestartGate({ queue.add(it) }, { restarts++ })
    gate.onResume()
    gate.request()
    gate.onPause()
    queue.removeAt(0)()
    assertEquals(0, restarts)
    gate.onResume()
    queue.removeAt(0)()
    assertEquals(1, restarts)
  }

  @Test fun activityTeardownCancelsAQueuedRestart() {
    val queue = mutableListOf<() -> Unit>()
    var restarts = 0
    val gate = RendererRestartGate({ queue.add(it) }, { restarts++ })
    gate.onResume()
    gate.request()
    gate.close()
    queue.removeAt(0)()
    assertEquals(0, restarts)
  }

  @Test fun compiledGeneratedClassesContainBothRecoveryHooks() {
    assertNotNull(RustWebViewClient::class.java.getDeclaredMethod(
      "onRenderProcessGone", WebView::class.java, RenderProcessGoneDetail::class.java,
    ))
    assertNotNull(WryActivity::class.java.getDeclaredMethod("releaseFailedWebView", WebView::class.java))
  }

  @Test fun failedViewDestructionCannotBeReportedAsHandled() {
    val coordinator = RendererRecoveryCoordinator { _, _ -> }
    assertFalse(coordinator.recover(
      removeFromParent = {}, removeJavascriptBridge = {},
      destroyView = { error("view still live") }, clearReference = {},
      markRecovery = {}, restart = { true },
    ))
  }
}
