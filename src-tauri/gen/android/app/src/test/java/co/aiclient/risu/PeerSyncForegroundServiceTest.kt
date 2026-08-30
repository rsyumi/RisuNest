package co.aiclient.risu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PeerSyncForegroundServiceTest {
  private val operationId = "11111111-1111-4111-8111-111111111111"

  @Test
  fun `only user started peer sync lanes are accepted`() {
    assertTrue(isAllowedPeerSyncForegroundLane("p1-source"))
    assertTrue(isAllowedPeerSyncForegroundLane("p4-source"))
    assertTrue(isAllowedPeerSyncForegroundLane("p4-target"))
    assertTrue(isAllowedPeerSyncForegroundLane("p5-source"))
    assertTrue(isAllowedPeerSyncForegroundLane("p5-target"))
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
    val p4Source = PeerSyncForegroundIdentity("p4-source", operationId, 8L)
    val p4Target = PeerSyncForegroundIdentity("p4-target", operationId, 9L)
    assertEquals(p4Source, peerSyncForegroundIdentity("p4-source", operationId, 8L))
    assertEquals(p4Target, peerSyncForegroundIdentity("p4-target", operationId, 9L))
    assertFalse(isExactAttachedPeerSyncStop(p4Source, p4Target))
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

  @Test
  fun `single service accepts only an idempotent sequential START until exact destruction`() {
    val p1 = PeerSyncForegroundIdentity("p1-source", operationId, 7L)
    val p4 = PeerSyncForegroundIdentity("p4-target", "22222222-2222-4222-8222-222222222222", 8L)
    assertTrue(canStartPeerSyncForeground(null, p1))
    assertTrue(canStartPeerSyncForeground(p1, p1))
    assertFalse(canStartPeerSyncForeground(p1, p4))
    assertEquals(p4, rejectedPeerSyncForegroundStart(p1, p4))
    assertEquals(null, rejectedPeerSyncForegroundStart(p1, p1))
    assertEquals(p1, peerSyncForegroundIdentityForDestruction(p1))
    assertEquals(null, peerSyncForegroundIdentityForDestruction(null))
  }

  @Test
  fun `accepted exact Stop clears Kotlin identity before native cross lane reserve`() {
    val p1 = PeerSyncForegroundIdentity("p1-source", operationId, 7L)
    val p4 = PeerSyncForegroundIdentity("p4-source", "22222222-2222-4222-8222-222222222222", 8L)
    var nativeOwner: PeerSyncForegroundIdentity? = p1
    var attached: PeerSyncForegroundIdentity? = p1
    assertEquals(p1, nativeOwner)

    attached = peerSyncForegroundIdentityAfterStop(attached, p1)
    nativeOwner = null
    assertEquals(null, attached)
    assertNull(nativeOwner)

    nativeOwner = p4
    assertTrue(canStartPeerSyncForeground(attached, nativeOwner))
    assertEquals(p4, nativeOwner)
  }

  @Test
  fun `stale P5 Stop retains Kotlin identity and blocks a fresh cross lane START`() {
    val p5 = PeerSyncForegroundIdentity("p5-source", operationId, 9L)
    val stale = p5.copy(generation = 8L)
    val fresh = PeerSyncForegroundIdentity(
      "p4-target",
      "22222222-2222-4222-8222-222222222222",
      10L,
    )

    val attached = peerSyncForegroundIdentityAfterStop(p5, stale)

    assertEquals(p5, attached)
    assertFalse(canStartPeerSyncForeground(attached, fresh))
  }
}
