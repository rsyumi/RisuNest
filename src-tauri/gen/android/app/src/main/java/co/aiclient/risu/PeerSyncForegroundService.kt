package co.aiclient.risu

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.util.UUID

internal const val PEER_SYNC_FOREGROUND_START_MODE = Service.START_NOT_STICKY
private const val PEER_SYNC_FOREGROUND_CHANNEL = "risu-peer-sync-source"
private const val PEER_SYNC_FOREGROUND_NOTIFICATION_ID = 0x52535031
private const val PEER_SYNC_FOREGROUND_START = "co.aiclient.risu.PEER_SYNC_SOURCE_START"
internal const val PEER_SYNC_FOREGROUND_STOP_ACTION = "co.aiclient.risu.PEER_SYNC_SOURCE_STOP"
internal const val PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS =
  PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
private const val PEER_SYNC_LANE_EXTRA = "lane"
private const val PEER_SYNC_OPERATION_ID_EXTRA = "operationId"
private const val PEER_SYNC_GENERATION_EXTRA = "generation"

internal data class PeerSyncForegroundIdentity(
  val lane: String,
  val operationId: String,
  val generation: Long,
)

internal fun isAllowedPeerSyncForegroundLane(lane: String): Boolean =
  lane == "p1-source" || lane == "p4-source" || lane == "p4-target"

internal fun peerSyncForegroundIdentity(
  lane: String?,
  operationId: String?,
  generation: Long,
): PeerSyncForegroundIdentity? {
  if (lane == null || !isAllowedPeerSyncForegroundLane(lane) || generation <= 0L) return null
  operationId ?: return null
  val canonical = runCatching { UUID.fromString(operationId).toString() }.getOrNull()
  if (canonical != operationId) return null
  return PeerSyncForegroundIdentity(lane, canonical, generation)
}

internal fun peerSyncForegroundIdentityExtras(
  lane: String,
  operationId: String,
  generation: Long,
): Map<String, Any> = mapOf(
  PEER_SYNC_LANE_EXTRA to lane,
  PEER_SYNC_OPERATION_ID_EXTRA to operationId,
  PEER_SYNC_GENERATION_EXTRA to generation,
)

internal fun isExactAttachedPeerSyncStop(
  attached: PeerSyncForegroundIdentity?,
  requested: PeerSyncForegroundIdentity,
): Boolean = attached == requested

internal fun peerSyncForegroundIdentityAfterStop(
  attached: PeerSyncForegroundIdentity?,
  requested: PeerSyncForegroundIdentity,
): PeerSyncForegroundIdentity? = attached.takeUnless { isExactAttachedPeerSyncStop(it, requested) }

internal fun canStartPeerSyncForeground(
  attached: PeerSyncForegroundIdentity?,
  requested: PeerSyncForegroundIdentity,
): Boolean = attached == null || attached == requested

internal fun rejectedPeerSyncForegroundStart(
  attached: PeerSyncForegroundIdentity?,
  requested: PeerSyncForegroundIdentity,
): PeerSyncForegroundIdentity? = requested.takeUnless { canStartPeerSyncForeground(attached, it) }

internal fun peerSyncForegroundIdentityForDestruction(
  attached: PeerSyncForegroundIdentity?,
): PeerSyncForegroundIdentity? = attached

internal object PeerSyncForegroundNativeBridge {
  init {
    System.loadLibrary("risuai_lib")
  }

  @JvmStatic external fun attach(lane: String, operationId: String, generation: Long): Boolean
  @JvmStatic external fun cancel(lane: String, operationId: String, generation: Long): Boolean
  @JvmStatic external fun detach(lane: String, operationId: String, generation: Long): Boolean
}

class PeerSyncForegroundService : Service() {
  private var attached: PeerSyncForegroundIdentity? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val identity = intent?.peerSyncForegroundIdentity() ?: run {
      stopSelfResult(startId)
      return PEER_SYNC_FOREGROUND_START_MODE
    }
    if (intent.action == PEER_SYNC_FOREGROUND_STOP_ACTION) {
      PeerSyncForegroundNativeBridge.cancel(identity.lane, identity.operationId, identity.generation)
      val exactAttachedStop = isExactAttachedPeerSyncStop(attached, identity)
      if (exactAttachedStop) {
        stopForeground(STOP_FOREGROUND_REMOVE)
        attached = peerSyncForegroundIdentityAfterStop(attached, identity)
      }
      if (attached == null || exactAttachedStop) stopSelfResult(startId)
      return PEER_SYNC_FOREGROUND_START_MODE
    }
    if (intent.action != PEER_SYNC_FOREGROUND_START) {
      stopSelfResult(startId)
      return PEER_SYNC_FOREGROUND_START_MODE
    }
    rejectedPeerSyncForegroundStart(attached, identity)?.let { rejected ->
      PeerSyncForegroundNativeBridge.cancel(rejected.lane, rejected.operationId, rejected.generation)
      PeerSyncForegroundNativeBridge.detach(rejected.lane, rejected.operationId, rejected.generation)
      return PEER_SYNC_FOREGROUND_START_MODE
    }
    if (attached == identity) return PEER_SYNC_FOREGROUND_START_MODE

    createNotificationChannel()
    startForeground(PEER_SYNC_FOREGROUND_NOTIFICATION_ID, notification(identity))
    if (!PeerSyncForegroundNativeBridge.attach(identity.lane, identity.operationId, identity.generation)) {
      stopForeground(STOP_FOREGROUND_REMOVE)
      stopSelfResult(startId)
      return PEER_SYNC_FOREGROUND_START_MODE
    }
    attached = identity
    return PEER_SYNC_FOREGROUND_START_MODE
  }

  override fun onDestroy() {
    peerSyncForegroundIdentityForDestruction(attached)?.let { identity ->
      PeerSyncForegroundNativeBridge.cancel(identity.lane, identity.operationId, identity.generation)
      PeerSyncForegroundNativeBridge.detach(identity.lane, identity.operationId, identity.generation)
    }
    attached = null
    super.onDestroy()
  }

  private fun notification(identity: PeerSyncForegroundIdentity): Notification {
    val stopIntent = identity.intent(this, PEER_SYNC_FOREGROUND_STOP_ACTION)
    val stopPendingIntent = PendingIntent.getService(
      this,
      identity.generation.hashCode(),
      stopIntent,
      PEER_SYNC_FOREGROUND_STOP_PENDING_FLAGS,
    )
    return NotificationCompat.Builder(this, PEER_SYNC_FOREGROUND_CHANNEL)
      .setSmallIcon(android.R.drawable.stat_sys_upload)
      .setContentTitle(getString(R.string.peer_sync_source_notification_title))
      .setContentText(getString(R.string.peer_sync_source_notification_text))
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .addAction(
        android.R.drawable.ic_menu_close_clear_cancel,
        getString(R.string.peer_sync_source_notification_stop),
        stopPendingIntent,
      )
      .build()
  }

  private fun createNotificationChannel() {
    getSystemService(NotificationManager::class.java).createNotificationChannel(
      NotificationChannel(
        PEER_SYNC_FOREGROUND_CHANNEL,
        getString(R.string.peer_sync_source_notification_channel),
        NotificationManager.IMPORTANCE_LOW,
      ),
    )
  }

  companion object {
    internal fun start(context: Context, identity: PeerSyncForegroundIdentity): Boolean = runCatching {
      ContextCompat.startForegroundService(
        context,
        identity.intent(context, PEER_SYNC_FOREGROUND_START),
      )
      true
    }.getOrDefault(false)

    internal fun stop(context: Context, identity: PeerSyncForegroundIdentity): Boolean = runCatching {
      context.startService(identity.intent(context, PEER_SYNC_FOREGROUND_STOP_ACTION)) != null
    }.getOrDefault(false)
  }
}

private fun PeerSyncForegroundIdentity.intent(context: Context, action: String) =
  Intent(context, PeerSyncForegroundService::class.java).apply {
    this.action = action
    putExtra(PEER_SYNC_LANE_EXTRA, lane)
    putExtra(PEER_SYNC_OPERATION_ID_EXTRA, operationId)
    putExtra(PEER_SYNC_GENERATION_EXTRA, generation)
  }

private fun Intent.peerSyncForegroundIdentity(): PeerSyncForegroundIdentity? =
  peerSyncForegroundIdentity(
    getStringExtra(PEER_SYNC_LANE_EXTRA),
    getStringExtra(PEER_SYNC_OPERATION_ID_EXTRA),
    getLongExtra(PEER_SYNC_GENERATION_EXTRA, 0L),
  )
