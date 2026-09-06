package io.github.rsyumi.risunest

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PeerCloneTransferTest {
  private val jobId = "11111111-1111-4111-8111-111111111111"

  @Test
  fun `peer clone client is enabled while background work remains API 34 only`() {
    assertEquals(true, BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT)
    assertEquals(
      PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER,
      peerCloneTransferMode(
        experimentalEnabled = BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT,
        sdkInt = 34,
      ),
    )
  }

  @Test
  fun `Android 13 uses the foreground native route without enabling background work`() {
    assertEquals(
      PeerCloneTransferMode.FOREGROUND,
      peerCloneTransferMode(experimentalEnabled = true, sdkInt = 33),
    )
    assertEquals(
      PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER,
      peerCloneTransferMode(experimentalEnabled = true, sdkInt = 34),
    )
    assertEquals(
      PeerCloneTransferMode.DISABLED,
      peerCloneTransferMode(experimentalEnabled = false, sdkInt = 34),
    )
  }

  @Test
  fun `Javascript bridge exposes only the bounded transfer contract`() {
    assertEquals("foreground", peerCloneTransferModeWire(PeerCloneTransferMode.FOREGROUND))
    assertEquals(
      "uidt",
      peerCloneTransferModeWire(PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER),
    )
    assertEquals("disabled", peerCloneTransferModeWire(PeerCloneTransferMode.DISABLED))
    assertEquals("scheduled", peerCloneScheduleResultWire(PeerCloneScheduleResult.SCHEDULED))
    assertEquals("disabled", peerCloneScheduleResultWire(PeerCloneScheduleResult.DISABLED))
    assertEquals("rejected", peerCloneScheduleResultWire(PeerCloneScheduleResult.INVALID_JOB_ID))
    assertEquals("rejected", peerCloneScheduleResultWire(PeerCloneScheduleResult.REJECTED))
  }

  @Test
  fun `scheduled job persistence contains only the opaque native job id`() {
    assertEquals(
      mapOf(PEER_CLONE_JOB_ID_EXTRA to jobId),
      peerClonePersistedExtras(jobId),
    )
    assertFalse(peerClonePersistedExtras(jobId).values.any { it.contains("http") })
  }

  @Test
  fun `job notification is attached before native transfer work starts`() {
    val operations = mutableListOf<String>()

    startPeerCloneTransfer(
      attachNotification = { operations.add("notification") },
      launchNativeTransfer = { operations.add("native") },
    )

    assertEquals(listOf("notification", "native"), operations)
  }

  @Test
  fun `system stop pauses native work and retains resumable state`() {
    assertEquals(
      PeerCloneStopDecision(
        nativeAction = PeerCloneNativeStopAction.PAUSE_RETAIN_STATE,
        shouldReschedule = true,
      ),
      peerCloneStopDecision(PeerCloneStopCause.SYSTEM),
    )
  }

  @Test
  fun `job callback cancellation marks native state without blocking on cleanup`() {
    listOf(PeerCloneStopCause.USER, PeerCloneStopCause.APP_CANCELLED).forEach { cause ->
      assertEquals(
        PeerCloneStopDecision(
          nativeAction = PeerCloneNativeStopAction.REQUEST_CANCEL_RETAIN_STATE,
          shouldReschedule = false,
        ),
        peerCloneStopDecision(cause),
      )
    }
  }

  @Test
  fun `explicit cancel stops scheduler before native cleanup`() {
    val operations = mutableListOf<String>()

    val cancelled = cancelPeerCloneTransfer(
      cancelScheduledJob = { operations.add("scheduler-cancel") },
      cancelAndCleanupNative = {
        operations.add("native-cleanup")
        true
      },
    )

    assertTrue(cancelled)
    assertEquals(listOf("scheduler-cancel", "native-cleanup"), operations)
  }

  @Test
  fun `failed native cleanup reports failure after scheduler cancellation`() {
    val operations = mutableListOf<String>()

    val cancelled = cancelPeerCloneTransfer(
      cancelScheduledJob = { operations.add("scheduler-cancel") },
      cancelAndCleanupNative = {
        operations.add("native-cleanup")
        false
      },
    )

    assertFalse(cancelled)
    assertEquals(listOf("scheduler-cancel", "native-cleanup"), operations)
  }

  @Test
  fun `concurrent clone jobs use distinct notification identities`() {
    val otherJobId = "22222222-2222-4222-8222-222222222222"

    assertTrue(peerCloneNotificationId(jobId) >= 0)
    assertTrue(peerCloneNotificationId(otherJobId) >= 0)
    assertFalse(peerCloneNotificationId(jobId) == peerCloneNotificationId(otherJobId))
  }

  @Test
  fun `native downloader progress reaches the notification updater`() {
    val updates = mutableListOf<Long>()

    val result = runPeerCloneNativeTransfer(
      resume = { progress ->
        progress.onProgress(64 * 1024L)
        progress.onProgress(4 * 1024 * 1024L)
        PeerCloneNativeResult.VERIFIED_AWAITING_ACTIVATION.wireCode
      },
      updateNotification = updates::add,
    )

    assertEquals(PeerCloneNativeResult.VERIFIED_AWAITING_ACTIVATION.wireCode, result)
    assertEquals(listOf(64 * 1024L, 4 * 1024 * 1024L), updates)
  }

  @Test
  fun `notification progress updates are bounded while preserving transfer status`() {
    val gate = PeerCloneNotificationProgress()

    assertTrue(gate.shouldUpdate(64 * 1024L))
    assertFalse(gate.shouldUpdate(1024 * 1024L))
    assertTrue(gate.shouldUpdate(4 * 1024 * 1024L + 64 * 1024L))
    assertFalse(gate.shouldUpdate(4 * 1024 * 1024L))
  }

  @Test
  fun `verified transfer removes its running notification but retains activation state`() {
    assertEquals(
      PeerCloneCompletionDecision(
        retainNativeState = true,
        shouldReschedule = false,
      ),
      peerCloneCompletionDecision(PeerCloneNativeResult.VERIFIED_AWAITING_ACTIVATION),
    )
    assertTrue(
      peerCloneCompletionDecision(PeerCloneNativeResult.RETRYABLE_INTERRUPTION)
        .shouldReschedule,
    )
  }

  @Test
  fun `terminal transfer failure retains bounded native status for explicit cleanup`() {
    assertEquals(
      PeerCloneCompletionDecision(
        retainNativeState = true,
        shouldReschedule = false,
      ),
      peerCloneCompletionDecision(PeerCloneNativeResult.TERMINAL_FAILURE),
    )
  }

  @Test
  fun `native resume exception is terminal and never requests UIDT rescheduling`() {
    val result = peerCloneNativeResult { error("JNI unavailable") }

    assertEquals(PeerCloneNativeResult.TERMINAL_FAILURE, result)
    assertFalse(peerCloneCompletionDecision(result).shouldReschedule)
  }

  @Test
  fun `activity lifecycle changes native admission only for foreground clone work`() {
    val transitions = mutableListOf<Pair<PeerCloneTransferMode, Boolean>>()

    PeerCloneTransferMode.entries.forEach { mode ->
      updateForegroundPeerCloneLifecycle(mode, foreground = true) {
        transitions.add(mode to it)
      }
      updateForegroundPeerCloneLifecycle(mode, foreground = false) {
        transitions.add(mode to it)
      }
    }

    assertEquals(
      listOf(
        PeerCloneTransferMode.FOREGROUND to true,
        PeerCloneTransferMode.FOREGROUND to false,
      ),
      transitions,
    )
  }
}
