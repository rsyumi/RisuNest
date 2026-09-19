package io.github.rsyumi.risunest

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidControlBridgeTest {
  @Test fun acceptsOnlyTheExactAppOriginInTheMainFrame() {
    for (origin in listOf("http://tauri.localhost", "https://tauri.localhost")) {
      assertTrue(acceptsAndroidControl(origin, true))
      assertFalse(acceptsAndroidControl(origin, false))
    }
  }

  @Test fun rejectsOpaquePluginAndLookalikeOrigins() {
    for (origin in listOf(
      "null", "about:blank", "file://", "https://example.invalid",
      "https://tauri.localhost.example.invalid", "https://tauri.localhost:5174",
      "https://example.invalid@tauri.localhost", "http://localhost:5174",
    )) {
      assertFalse(origin, acceptsAndroidControl(origin, true))
      assertFalse(origin, acceptsAndroidControl(origin, false))
    }
  }

  @Test fun developmentOriginsAreExactAndNeverAllowedInRelease() {
    val development = "http://192.0.2.1:5174"
    assertTrue(acceptsAndroidControl(development, true, androidControlOrigins(true, development)))
    assertFalse(acceptsAndroidControl(development, false, androidControlOrigins(true, development)))
    assertFalse(acceptsAndroidControl(development, true, androidControlOrigins(false, development)))
    assertFalse(acceptsAndroidControl("http://192.0.2.1:80", true, androidControlOrigins(true, development)))
    for (invalid in listOf("*", "null", "http://*.example.invalid", "http://localhost:5174/path", "http://user@localhost:5174")) {
      assertEquals(CONTROL_ORIGINS, androidControlOrigins(true, invalid))
    }
  }

  @Test fun onlyEventBasedOperationsCanOmitAReply() {
    assertTrue(androidControlNotification("lifecycle.onFlushHold"))
    assertTrue(androidControlNotification("saf.copyExport"))
    assertFalse(androidControlNotification("saf.acknowledgeExport"))
    assertFalse(androidControlNotification("generation.begin"))
  }

  @Test fun dispatchSurfaceIsExplicitAndKeepsExistingArgumentShapes() {
    assertEquals(0, androidControlArgumentCount("generation.begin"))
    assertEquals(1, androidControlArgumentCount("lifecycle.onFlushHold"))
    assertEquals(1, androidControlArgumentCount("saf.acknowledgeExport"))
    assertEquals(2, androidControlArgumentCount("saf.pickContentSource"))
    assertEquals(3, androidControlArgumentCount("saf.copyExport"))
    assertNull(androidControlArgumentCount("saf.deleteFile"))
    assertNull(androidControlArgumentCount("lifecycle.finishAndRemoveTask"))
    assertNull(androidControlArgumentCount("generation.*"))
  }
}
