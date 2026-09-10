package io.github.rsyumi.risunest

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

internal const val GENERATION_FOREGROUND_START_MODE = Service.START_NOT_STICKY
private const val GENERATION_FOREGROUND_CHANNEL = "risunest-generation"
private const val GENERATION_FOREGROUND_NOTIFICATION_ID = 0x52474e31
private const val GENERATION_FOREGROUND_BEGIN = "io.github.rsyumi.risunest.GENERATION_FOREGROUND_BEGIN"
private const val GENERATION_FOREGROUND_TOKEN = "io.github.rsyumi.risunest.GENERATION_FOREGROUND_TOKEN"
private const val INVALID_GENERATION_FOREGROUND_TOKEN = -1L

internal class GenerationForegroundLifecycle {
  private var count = 0
  private var activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
  private var activeStartId: Int? = null
  private var nextToken = 0L

  @Synchronized
  fun begin(dispatchStart: (Long) -> Boolean): Boolean {
    if (count > 0) {
      count += 1
      return true
    }

    nextToken += 1
    val token = nextToken
    if (!dispatchStart(token)) return false
    activeToken = token
    count = 1
    return true
  }

  @Synchronized
  fun end(stopService: () -> Boolean): Boolean {
    if (count == 0) return false
    count -= 1
    if (count > 0) return true

    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    return stopService()
  }

  @Synchronized
  fun stopAll(stopService: () -> Boolean): Boolean {
    if (count == 0) return false
    count = 0
    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    return stopService()
  }

  @Synchronized
  fun timeout(
    token: Long,
    startId: Int,
    stopTimedOut: () -> Unit,
    stopStaleTimeout: () -> Unit,
  ): Boolean {
    if (token != activeToken || startId != activeStartId) {
      stopStaleTimeout()
      return false
    }
    count = 0
    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    stopTimedOut()
    return true
  }

  @Synchronized
  fun serviceDestroyed(token: Long, startId: Int): Boolean {
    if (token != activeToken || startId != activeStartId) return false
    count = 0
    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    return true
  }

  @Synchronized
  fun activate(
    token: Long,
    startId: Int,
    startForeground: () -> Unit,
    stopStaleStart: () -> Unit,
  ): Boolean {
    if (count > 0 && token == activeToken) {
      activeStartId = startId
      startForeground()
      return true
    }
    stopStaleStart()
    return false
  }
}

class GenerationForegroundService : Service() {
  private var activatedToken = INVALID_GENERATION_FOREGROUND_TOKEN
  private var activatedStartId: Int? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val token = if (intent?.action == GENERATION_FOREGROUND_BEGIN) {
      intent.getLongExtra(GENERATION_FOREGROUND_TOKEN, INVALID_GENERATION_FOREGROUND_TOKEN)
    } else {
      INVALID_GENERATION_FOREGROUND_TOKEN
    }
    lifecycle.activate(
      token = token,
      startId = startId,
      startForeground = {
        activatedToken = token
        activatedStartId = startId
        startInForeground()
      },
      stopStaleStart = {
        stopSelfResult(startId)
      },
    )
    return GENERATION_FOREGROUND_START_MODE
  }

  override fun onDestroy() {
    activatedStartId?.let { startId ->
      lifecycle.serviceDestroyed(activatedToken, startId)
    }
    super.onDestroy()
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    lifecycle.timeout(
      token = activatedToken,
      startId = startId,
      stopTimedOut = {
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelfResult(startId)
      },
      stopStaleTimeout = { stopSelfResult(startId) },
    )
  }

  private fun startInForeground() {
    createNotificationChannel()
    val openAppIntent = PendingIntent.getActivity(
      this,
      GENERATION_FOREGROUND_NOTIFICATION_ID,
      Intent(this, MainActivity::class.java).addFlags(
        Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP,
      ),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    startForeground(
      GENERATION_FOREGROUND_NOTIFICATION_ID,
      androidx.core.app.NotificationCompat.Builder(this, GENERATION_FOREGROUND_CHANNEL)
        .setSmallIcon(android.R.drawable.stat_sys_download)
        .setContentTitle(getString(R.string.generation_notification_title))
        .setContentText(getString(R.string.generation_notification_text))
        .setContentIntent(openAppIntent)
        .setOngoing(true)
        .setOnlyAlertOnce(true)
        .build(),
    )
  }

  private fun createNotificationChannel() {
    NotificationManagerCompat.from(this).createNotificationChannel(
      NotificationChannelCompat.Builder(
        GENERATION_FOREGROUND_CHANNEL,
        NotificationManagerCompat.IMPORTANCE_LOW,
      )
        .setName(getString(R.string.generation_notification_channel))
        .build(),
    )
  }

  companion object {
    private val lifecycle = GenerationForegroundLifecycle()

    internal fun start(context: Context): Boolean = lifecycle.begin { token ->
      runCatching {
        ContextCompat.startForegroundService(
          context,
          Intent(context, GenerationForegroundService::class.java)
            .setAction(GENERATION_FOREGROUND_BEGIN)
            .putExtra(GENERATION_FOREGROUND_TOKEN, token),
        )
        true
      }.getOrDefault(false)
    }

    internal fun stop(context: Context): Boolean = lifecycle.end {
      runCatching {
        context.stopService(Intent(context, GenerationForegroundService::class.java))
      }.getOrDefault(false)
    }

    internal fun stopAll(context: Context): Boolean = lifecycle.stopAll {
      runCatching {
        context.stopService(Intent(context, GenerationForegroundService::class.java))
      }.getOrDefault(false)
    }

    internal fun notificationsEnabled(context: Context): Boolean {
      val manager = NotificationManagerCompat.from(context)
      if (!manager.areNotificationsEnabled()) return false
      val channel = manager.getNotificationChannelCompat(GENERATION_FOREGROUND_CHANNEL)
      return channel == null || channel.importance != NotificationManagerCompat.IMPORTANCE_NONE
    }
  }
}
