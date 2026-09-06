package io.github.rsyumi.risunest

import org.junit.Assert.assertEquals
import org.junit.Test

class BackNavigationPolicyTest {
  @Test
  fun `web history takes priority over app exit`() {
    val policy = BackNavigationPolicy()

    assertEquals(BackNavigationAction.GO_BACK, policy.decide(canGoBack = true, nowMillis = 1_000))
  }

  @Test
  fun `first back press at the root shows the exit hint`() {
    val policy = BackNavigationPolicy()

    assertEquals(BackNavigationAction.SHOW_EXIT_HINT, policy.decide(canGoBack = false, nowMillis = 1_000))
  }

  @Test
  fun `second root back press within two seconds exits`() {
    val policy = BackNavigationPolicy()
    policy.decide(canGoBack = false, nowMillis = 1_000)

    assertEquals(BackNavigationAction.EXIT, policy.decide(canGoBack = false, nowMillis = 3_000))
  }

  @Test
  fun `root back press after the confirmation window shows the hint again`() {
    val policy = BackNavigationPolicy()
    policy.decide(canGoBack = false, nowMillis = 1_000)

    assertEquals(BackNavigationAction.SHOW_EXIT_HINT, policy.decide(canGoBack = false, nowMillis = 3_001))
  }

  @Test
  fun `web history navigation clears an armed exit`() {
    val policy = BackNavigationPolicy()
    policy.decide(canGoBack = false, nowMillis = 1_000)
    policy.decide(canGoBack = true, nowMillis = 1_500)

    assertEquals(BackNavigationAction.SHOW_EXIT_HINT, policy.decide(canGoBack = false, nowMillis = 2_000))
  }
}
