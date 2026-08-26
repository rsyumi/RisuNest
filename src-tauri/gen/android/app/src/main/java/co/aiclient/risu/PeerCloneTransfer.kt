package co.aiclient.risu

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.PersistableBundle
import androidx.annotation.RequiresApi
import androidx.core.app.NotificationCompat
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

internal const val PEER_CLONE_JOB_ID_EXTRA = "nativePeerCloneJobId"
private const val PEER_CLONE_NOTIFICATION_CHANNEL = "risunest-peer-clone"
private const val PEER_CLONE_CANCEL_ACTION = "co.aiclient.risu.CANCEL_PEER_CLONE"

internal enum class PeerCloneTransferMode {
  DISABLED,
  UNSUPPORTED_ANDROID_VERSION,
  USER_INITIATED_DATA_TRANSFER,
}

internal fun peerCloneTransferMode(
  experimentalEnabled: Boolean,
  sdkInt: Int,
): PeerCloneTransferMode = when {
  !experimentalEnabled -> PeerCloneTransferMode.DISABLED
  sdkInt < 34 -> PeerCloneTransferMode.UNSUPPORTED_ANDROID_VERSION
  else -> PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER
}

internal fun peerClonePersistedExtras(jobId: String): Map<String, String> =
  mapOf(PEER_CLONE_JOB_ID_EXTRA to jobId)

internal inline fun startPeerCloneTransfer(
  attachNotification: () -> Unit,
  launchNativeTransfer: () -> Unit,
) {
  attachNotification()
  launchNativeTransfer()
}

internal inline fun cancelPeerCloneTransfer(
  cancelAndCleanupNative: () -> Boolean,
): Boolean = cancelAndCleanupNative()

internal fun interface PeerCloneNativeProgress {
  fun onProgress(transferredBytes: Long)
}

internal fun runPeerCloneNativeTransfer(
  resume: (PeerCloneNativeProgress) -> Int,
  updateNotification: (Long) -> Unit,
): Int = resume(PeerCloneNativeProgress(updateNotification))

internal class PeerCloneNotificationProgress(
  private val minimumStepBytes: Long = 4 * 1024 * 1024L,
) {
  private var lastReportedBytes = 0L

  fun shouldUpdate(transferredBytes: Long): Boolean {
    if (transferredBytes <= lastReportedBytes) return false
    if (lastReportedBytes != 0L && transferredBytes - lastReportedBytes < minimumStepBytes) {
      return false
    }
    lastReportedBytes = transferredBytes
    return true
  }
}

internal enum class PeerCloneStopCause {
  SYSTEM,
  USER,
  APP_CANCELLED,
}

internal enum class PeerCloneNativeStopAction {
  PAUSE_RETAIN_STATE,
  CANCEL_AND_CLEANUP,
}

internal data class PeerCloneStopDecision(
  val nativeAction: PeerCloneNativeStopAction,
  val shouldReschedule: Boolean,
)

internal fun peerCloneStopDecision(cause: PeerCloneStopCause) = when (cause) {
  PeerCloneStopCause.SYSTEM -> PeerCloneStopDecision(
    nativeAction = PeerCloneNativeStopAction.PAUSE_RETAIN_STATE,
    shouldReschedule = true,
  )
  PeerCloneStopCause.USER,
  PeerCloneStopCause.APP_CANCELLED,
  -> PeerCloneStopDecision(
    nativeAction = PeerCloneNativeStopAction.CANCEL_AND_CLEANUP,
    shouldReschedule = false,
  )
}

internal enum class PeerCloneNativeResult(val wireCode: Int) {
  COMPLETED_ACTIVATED(0),
  RETRYABLE_INTERRUPTION(1),
  VERIFIED_AWAITING_ACTIVATION(2),
  CANCELLED(3),
  TERMINAL_FAILURE(4),
  ;

  companion object {
    fun fromWireCode(value: Int) = entries.firstOrNull { it.wireCode == value }
      ?: TERMINAL_FAILURE
  }
}

internal data class PeerCloneCompletionDecision(
  val retainNativeState: Boolean,
  val shouldReschedule: Boolean,
)

internal fun peerCloneCompletionDecision(result: PeerCloneNativeResult) = when (result) {
  PeerCloneNativeResult.RETRYABLE_INTERRUPTION -> PeerCloneCompletionDecision(
    retainNativeState = true,
    shouldReschedule = true,
  )
  PeerCloneNativeResult.VERIFIED_AWAITING_ACTIVATION -> PeerCloneCompletionDecision(
    retainNativeState = true,
    shouldReschedule = false,
  )
  PeerCloneNativeResult.COMPLETED_ACTIVATED,
  PeerCloneNativeResult.CANCELLED,
  PeerCloneNativeResult.TERMINAL_FAILURE,
  -> PeerCloneCompletionDecision(
    retainNativeState = false,
    shouldReschedule = false,
  )
}

internal object PeerCloneNativeBridge {
  init {
    System.loadLibrary("risuai_lib")
  }

  @JvmStatic external fun resume(
    jobId: String,
    filesRoot: String,
    progress: PeerCloneNativeProgress,
  ): Int
  @JvmStatic external fun pause(jobId: String): Boolean
  @JvmStatic external fun cancelAndCleanup(jobId: String, filesRoot: String): Boolean
  @JvmStatic external fun cleanupCompleted(jobId: String, filesRoot: String): Boolean
}

internal enum class PeerCloneScheduleResult {
  SCHEDULED,
  DISABLED,
  UNSUPPORTED_ANDROID_VERSION,
  INVALID_JOB_ID,
  REJECTED,
}

internal object PeerCloneTransferScheduler {
  fun schedule(context: Context, jobId: String, expectedDownloadBytes: Long?): PeerCloneScheduleResult {
    if (!isCanonicalUuidV4(jobId)) return PeerCloneScheduleResult.INVALID_JOB_ID
    return when (
      peerCloneTransferMode(
        experimentalEnabled = BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT,
        sdkInt = Build.VERSION.SDK_INT,
      )
    ) {
      PeerCloneTransferMode.DISABLED -> PeerCloneScheduleResult.DISABLED
      PeerCloneTransferMode.UNSUPPORTED_ANDROID_VERSION ->
        PeerCloneScheduleResult.UNSUPPORTED_ANDROID_VERSION
      PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER ->
        scheduleApi34(context, jobId, expectedDownloadBytes)
    }
  }

  fun cancel(context: Context, jobId: String): Boolean {
    if (!BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT || !isCanonicalUuidV4(jobId)) {
      return false
    }
    return cancelPeerCloneTransfer(
      cancelAndCleanupNative = {
        runCatching { PeerCloneNativeBridge.cancelAndCleanup(jobId, context.filesDir.absolutePath) }
          .getOrDefault(false)
      },
    )
  }

  @RequiresApi(34)
  private fun scheduleApi34(
    context: Context,
    jobId: String,
    expectedDownloadBytes: Long?,
  ): PeerCloneScheduleResult {
    val extras = PersistableBundle().apply {
      peerClonePersistedExtras(jobId).forEach(::putString)
    }
    val downloadBytes = expectedDownloadBytes?.takeIf { it >= 0 }
      ?: JobInfo.NETWORK_BYTES_UNKNOWN.toLong()
    val info = JobInfo.Builder(
      peerCloneSchedulerId(jobId),
      ComponentName(context, PeerCloneTransferJobService::class.java),
    )
      .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
      .setEstimatedNetworkBytes(downloadBytes, 0L)
      .setExtras(extras)
      .setUserInitiated(true)
      .build()
    return if (context.getSystemService(JobScheduler::class.java).schedule(info) == JobScheduler.RESULT_SUCCESS) {
      PeerCloneScheduleResult.SCHEDULED
    } else {
      PeerCloneScheduleResult.REJECTED
    }
  }
}

class PeerCloneTransferJobService : JobService() {
  private val transferScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
  private val activeJobs = ConcurrentHashMap<Int, String>()

  override fun onStartJob(params: JobParameters): Boolean {
    if (
      peerCloneTransferMode(
        BuildConfig.ENABLE_EXPERIMENTAL_PEER_CLONE_CLIENT,
        Build.VERSION.SDK_INT,
      ) != PeerCloneTransferMode.USER_INITIATED_DATA_TRANSFER
    ) {
      return false
    }
    val jobId = params.extras.getString(PEER_CLONE_JOB_ID_EXTRA)
      ?.takeIf(::isCanonicalUuidV4)
      ?: return false
    activeJobs[params.jobId] = jobId
    startPeerCloneTransfer(
      attachNotification = { attachTransferNotification(params, jobId) },
      launchNativeTransfer = {
        transferScope.launch {
          val notificationProgress = PeerCloneNotificationProgress()
          val result = runCatching {
            runPeerCloneNativeTransfer(
              resume = { progress ->
                PeerCloneNativeBridge.resume(jobId, filesDir.absolutePath, progress)
              },
              updateNotification = { transferredBytes ->
                if (
                  activeJobs[params.jobId] == jobId &&
                  notificationProgress.shouldUpdate(transferredBytes)
                ) {
                  attachTransferNotification(params, jobId, transferredBytes)
                }
              },
            )
          }
            .fold(
              onSuccess = PeerCloneNativeResult::fromWireCode,
              onFailure = { PeerCloneNativeResult.RETRYABLE_INTERRUPTION },
            )
          val decision = peerCloneCompletionDecision(result)
          if (activeJobs.remove(params.jobId, jobId)) {
            if (!decision.retainNativeState) {
              runCatching { PeerCloneNativeBridge.cleanupCompleted(jobId, filesDir.absolutePath) }
            }
            jobFinished(params, decision.shouldReschedule)
          }
        }
      },
    )
    return true
  }

  override fun onStopJob(params: JobParameters): Boolean {
    val jobId = activeJobs.remove(params.jobId)
      ?: params.extras.getString(PEER_CLONE_JOB_ID_EXTRA)?.takeIf(::isCanonicalUuidV4)
      ?: return false
    val decision = peerCloneStopDecision(stopCause(params))
    when (decision.nativeAction) {
      PeerCloneNativeStopAction.PAUSE_RETAIN_STATE ->
        runCatching { PeerCloneNativeBridge.pause(jobId) }
      PeerCloneNativeStopAction.CANCEL_AND_CLEANUP ->
        runCatching { PeerCloneNativeBridge.cancelAndCleanup(jobId, filesDir.absolutePath) }
    }
    return decision.shouldReschedule
  }

  override fun onDestroy() {
    transferScope.cancel()
    super.onDestroy()
  }

  @SuppressLint("MissingPermission")
  private fun attachTransferNotification(
    params: JobParameters,
    jobId: String,
    transferredBytes: Long? = null,
  ) {
    if (Build.VERSION.SDK_INT < 34) return
    val manager = getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(
      NotificationChannel(
        PEER_CLONE_NOTIFICATION_CHANNEL,
        getString(R.string.peer_clone_notification_channel),
        NotificationManager.IMPORTANCE_LOW,
      ),
    )
    setNotification(
      params,
      peerCloneNotificationId(jobId),
      transferNotification(jobId, transferredBytes),
      JOB_END_NOTIFICATION_POLICY_REMOVE,
    )
  }

  private fun transferNotification(jobId: String, transferredBytes: Long?): Notification {
    val cancelIntent = Intent(this, PeerCloneCancelReceiver::class.java).apply {
      action = PEER_CLONE_CANCEL_ACTION
      putExtra(PEER_CLONE_JOB_ID_EXTRA, jobId)
    }
    val cancelPendingIntent = PendingIntent.getBroadcast(
      this,
      peerCloneSchedulerId(jobId),
      cancelIntent,
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val status = transferredBytes?.let {
      getString(R.string.peer_clone_notification_progress, it)
    } ?: getString(R.string.peer_clone_notification_text)
    return NotificationCompat.Builder(this, PEER_CLONE_NOTIFICATION_CHANNEL)
      .setSmallIcon(android.R.drawable.stat_sys_download)
      .setContentTitle(getString(R.string.peer_clone_notification_title))
      .setContentText(status)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setProgress(0, 0, true)
      .addAction(
        android.R.drawable.ic_menu_close_clear_cancel,
        getString(R.string.peer_clone_notification_cancel),
        cancelPendingIntent,
      )
      .build()
  }

  private fun stopCause(params: JobParameters): PeerCloneStopCause {
    if (Build.VERSION.SDK_INT < 31) return PeerCloneStopCause.SYSTEM
    return when (params.stopReason) {
      JobParameters.STOP_REASON_USER -> PeerCloneStopCause.USER
      JobParameters.STOP_REASON_CANCELLED_BY_APP -> PeerCloneStopCause.APP_CANCELLED
      else -> PeerCloneStopCause.SYSTEM
    }
  }
}

class PeerCloneCancelReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    if (intent.action != PEER_CLONE_CANCEL_ACTION) return
    val jobId = intent.getStringExtra(PEER_CLONE_JOB_ID_EXTRA) ?: return
    val pendingResult = goAsync()
    CoroutineScope(SupervisorJob() + Dispatchers.IO).launch {
      try {
        PeerCloneTransferScheduler.cancel(context.applicationContext, jobId)
      } finally {
        pendingResult.finish()
      }
    }
  }
}

private fun peerCloneSchedulerId(jobId: String): Int =
  UUID.fromString(jobId).hashCode() and Int.MAX_VALUE

internal fun peerCloneNotificationId(jobId: String): Int = peerCloneSchedulerId(jobId)
