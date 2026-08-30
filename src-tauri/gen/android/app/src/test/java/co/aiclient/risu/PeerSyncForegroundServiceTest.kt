package co.aiclient.risu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PeerSyncForegroundServiceTest {
  private val operationId = "11111111-1111-4111-8111-111111111111"

  @Test
  fun `only the P1 source lane is accepted`() {
    assertTrue(isAllowedPeerSyncForegroundLane("p1-source"))
    assertFalse(isAllowedPeerSyncForegroundLane("p3-target"))
    assertFalse(isAllowedPeerSyncForegroundLane("quick-tunnel"))
  }

  @Test
  fun `service extras contain identity only and no pairing secrets`() {
    val extras = peerSyncForegroundIdentityExtras("p1-source", operationId, 7L)
    assertEquals(setOf("lane", "operationId", "generation"), extras.keys)
    assertFalse(extras.keys.any { it.contains("token", true) || it.contains("bearer", true) || it.contains("claim", true) })
  }

  @Test
  fun `notification stop is immutable and generation exact`() {
    val current = PeerSyncForegroundIdentity("p1-source", operationId, 7L)
    assertEquals(current, peerSyncForegroundIdentity("p1-source", operationId, 7L))
    assertEquals(null, peerSyncForegroundIdentity("p1-source", operationId, 0L))
    assertEquals(null, peerSyncForegroundIdentity("p3-target", operationId, 7L))
    assertTrue(isExactAttachedPeerSyncStop(current, current))
    assertFalse(isExactAttachedPeerSyncStop(current, current.copy(generation = 6L)))
  }

  @Test
  fun `notification uses the explicit Stop action with immutable identity-only intent`() {
    assertEquals("co.aiclient.risu.PEER_SYNC_SOURCE_STOP", PEER_SYNC_FOREGROUND_STOP_ACTION)
    assertTrue(PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS and android.app.PendingIntent.FLAG_IMMUTABLE != 0)
    assertEquals(0, PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS and android.app.PendingIntent.FLAG_MUTABLE)
    assertEquals(
      setOf("lane", "operationId", "generation"),
      peerSyncForegroundIdentityExtras("p1-source", operationId, 7L).keys,
    )
  }

  @Test
  fun `foreground service never asks Android to restart it implicitly`() {
    assertEquals(android.app.Service.START_NOT_STICKY, PEER_SYNC_FOREGROUND_START_MODE)
  }
}
