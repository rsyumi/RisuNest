package io.github.rsyumi.risunest

import org.junit.Assert.*
import org.junit.Test

class AndroidCommitBridgeTest {
  @Test fun requiresBothWebViewFeatures() {
    assertTrue(supportsBinaryCommit(true, true))
    assertFalse(supportsBinaryCommit(true, false))
    assertFalse(supportsBinaryCommit(false, true))
    assertFalse(supportsBinaryCommit(false, false))
  }
  @Test fun onlyAcceptsExactAppOriginsAndMainFrame() {
    for (origin in COMMIT_ORIGINS) {
      assertTrue(acceptsBinaryCommit(origin, true))
      assertFalse(acceptsBinaryCommit(origin, false))
    }
    for (origin in listOf("null", "https://tauri.localhost.evil", "http://tauri.localhost:1234", "https://example.invalid", "http://localhost:5174")) {
      assertFalse(acceptsBinaryCommit(origin, true))
    }
  }
}
