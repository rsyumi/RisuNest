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
    assertTrue(isAllowedPeerSyncForegroundLane("device-sync-source"))
    assertTrue(isAllowedPeerSyncForegroundLane("p4-target"))
    assertTrue(isAllowedPeerSyncForegroundLane("p5-target"))
    assertFalse(isAllowedPeerSyncForegroundLane("p1-source"))
    assertFalse(isAllowedPeerSyncForegroundLane("p4-source"))
    assertFalse(isAllowedPeerSyncForegroundLane("p5-source"))
    assertFalse(isAllowedPeerSyncForegroundLane("p3-target"))
    assertFalse(isAllowedPeerSyncForegroundLane("quick-tunnel"))
  }

  @Test
  fun `unified source identity carries no endpoint or credential`() {
    val extras = peerSyncForegroundIdentityExtras("device-sync-source", operationId, 12L)
    assertEquals(setOf("lane", "operationId", "generation"), extras.keys)
    assertFalse(extras.keys.any {
      it.contains("endpoint", true) || it.contains("bearer", true) ||
        it.contains("claim", true) || it.contains("token", true)
    })
  }

  @Test
  fun `service extras contain identity only and no pairing secrets`() {
    val extras = peerSyncForegroundIdentityExtras("p4-target", operationId, 7L)
    assertEquals(setOf("lane", "operationId", "generation"), extras.keys)
    assertFalse(extras.keys.any { it.contains("token", true) || it.contains("bearer", true) || it.contains("claim", true) })
  }

  @Test
  fun `notification stop is immutable and generation exact`() {
    val current = PeerSyncForegroundIdentity("device-sync-source", operationId, 7L)
    assertEquals(current, peerSyncForegroundIdentity("device-sync-source", operationId, 7L))
    assertEquals(null, peerSyncForegroundIdentity("device-sync-source", operationId, 0L))
    assertEquals(null, peerSyncForegroundIdentity("p3-target", operationId, 7L))
    assertTrue(isExactAttachedPeerSyncStop(current, current))
    assertFalse(isExactAttachedPeerSyncStop(current, current.copy(generation = 6L)))
    val unifiedSource = PeerSyncForegroundIdentity("device-sync-source", operationId, 8L)
    val p4Target = PeerSyncForegroundIdentity("p4-target", operationId, 9L)
    assertEquals(unifiedSource, peerSyncForegroundIdentity("device-sync-source", operationId, 8L))
    assertEquals(p4Target, peerSyncForegroundIdentity("p4-target", operationId, 9L))
    assertFalse(isExactAttachedPeerSyncStop(unifiedSource, p4Target))
  }

  @Test
  fun `notification stop pending intent is immutable`() {
    assertTrue(PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS and android.app.PendingIntent.FLAG_IMMUTABLE != 0)
    assertEquals(0, PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS and android.app.PendingIntent.FLAG_MUTABLE)
  }

  @Test
  fun `foreground service never asks Android to restart it implicitly`() {
    assertEquals(android.app.Service.START_NOT_STICKY, PEER_SYNC_FOREGROUND_START_MODE)
  }

  @Test
  fun `single service accepts only an idempotent sequential START until exact destruction`() {
    val source = PeerSyncForegroundIdentity("device-sync-source", operationId, 7L)
    val p4 = PeerSyncForegroundIdentity("p4-target", "22222222-2222-4222-8222-222222222222", 8L)
    assertTrue(canStartPeerSyncForeground(null, source))
    assertTrue(canStartPeerSyncForeground(source, source))
    assertFalse(canStartPeerSyncForeground(source, p4))
    assertEquals(p4, rejectedPeerSyncForegroundStart(source, p4))
    assertEquals(null, rejectedPeerSyncForegroundStart(source, source))
  }

  @Test
  fun `accepted exact Stop clears the attached identity and admits a cross lane START`() {
    val source = PeerSyncForegroundIdentity("device-sync-source", operationId, 7L)
    val p4 = PeerSyncForegroundIdentity("p4-target", "22222222-2222-4222-8222-222222222222", 8L)

    val attached = peerSyncForegroundIdentityAfterStop(source, source)

    assertNull(attached)
    assertTrue(canStartPeerSyncForeground(attached, p4))
  }

  @Test
  fun `stale P5 Stop retains Kotlin identity and blocks a fresh cross lane START`() {
    val p5 = PeerSyncForegroundIdentity("p5-target", operationId, 9L)
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

  @Test
  fun `stale unified source Stop cannot detach a restarted generation`() {
    val current = PeerSyncForegroundIdentity("device-sync-source", operationId, 12L)
    val stale = current.copy(generation = 11L)

    assertEquals(current, peerSyncForegroundIdentityAfterStop(current, stale))
    assertTrue(canStartPeerSyncForeground(current, current))
    assertFalse(canStartPeerSyncForeground(current, current.copy(generation = 13L)))
  }
}
