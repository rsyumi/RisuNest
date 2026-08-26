package co.aiclient.risu

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PeerCloneTransferTest {
  private val jobId = "11111111-1111-4111-8111-111111111111"

  @Test
  fun `peer clone stays disabled until physical Android lifecycle validation`() {
    assertEquals(false, BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT)
    assertEquals(
      PeerCloneTransferMode.DISABLED,
      peerCloneTransferMode(
        experimentalEnabled = BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT,
        sdkInt = 34,
      ),
    )
  }

  @Test
  fun `Android 13 remains unsupported without a foreground service fallback`() {
    assertEquals(
      PeerCloneTransferMode.UNSUPPORTED_ANDROID_VERSION,
      peerCloneTransferMode(experimentalEnabled = true, sdkInt = 33),
    )
    assertEquals(
      PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER,
      peerCloneTransferMode(experimentalEnabled = true, sdkInt = 34),
    )
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
  fun `user and app cancellation clean native state and never reschedule`() {
    listOf(PeerCloneStopCause.USER, PeerCloneStopCause.APP_CANCELLED).forEach { cause ->
      assertEquals(
        PeerCloneStopDecision(
          nativeAction = PeerCloneNativeStopAction.CANCEL_AND_CLEANUP,
          shouldReschedule = false,
        ),
        peerCloneStopDecision(cause),
      )
    }
  }

  @Test
  fun `explicit cancel cleans the native job before cancelling its scheduler slot`() {
    val operations = mutableListOf<String>()

    cancelPeerCloneTransfer(
      cancelAndCleanupNative = { operations.add("native-cleanup") },
      cancelScheduledJob = { operations.add("scheduler-cancel") },
    )

    assertEquals(listOf("native-cleanup", "scheduler-cancel"), operations)
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
}
