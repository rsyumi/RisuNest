package co.aiclient.risu

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

internal const val GENERATION_FOREGROUND_START_MODE = Service.START_NOT_STICKY
private const val GENERATION_FOREGROUND_CHANNEL = "risunest-generation"
private const val GENERATION_FOREGROUND_NOTIFICATION_ID = 0x52474e31
private const val GENERATION_FOREGROUND_BEGIN = "co.aiclient.risu.GENERATION_FOREGROUND_BEGIN"
private const val GENERATION_FOREGROUND_END = "co.aiclient.risu.GENERATION_FOREGROUND_END"

internal enum class GenerationForegroundCommand { START, STOP, NONE }

internal class GenerationForegroundDispatchGate {
  private var count = 0

  @Synchronized
  fun begin(dispatch: () -> Boolean): Boolean {
    count += 1
    if (dispatch()) return true
    count -= 1
    return false
  }

  @Synchronized
  fun end(dispatch: () -> Boolean): Boolean {
    if (count == 0) return false
    count -= 1
    if (dispatch()) return true
    count += 1
    return false
  }

  @Synchronized
  fun timeout() {
    count = 0
  }
}

internal class GenerationForegroundController {
  private var count = 0

  fun begin(notificationsEnabled: Boolean): GenerationForegroundCommand {
    if (!notificationsEnabled) return GenerationForegroundCommand.NONE
    count += 1
    return if (count == 1) GenerationForegroundCommand.START else GenerationForegroundCommand.NONE
  }

  fun end(): GenerationForegroundCommand {
    if (count == 0) return GenerationForegroundCommand.NONE
    count -= 1
    return if (count == 0) GenerationForegroundCommand.STOP else GenerationForegroundCommand.NONE
  }

  fun timeout(): GenerationForegroundCommand {
    if (count == 0) return GenerationForegroundCommand.NONE
    count = 0
    return GenerationForegroundCommand.STOP
  }
}

class GenerationForegroundService : Service() {
  private val controller = GenerationForegroundController()

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val command = when (intent?.action) {
      GENERATION_FOREGROUND_BEGIN -> controller.begin(notificationsEnabled(this))
      GENERATION_FOREGROUND_END -> controller.end()
      else -> GenerationForegroundCommand.STOP
    }
    apply(command, startId)
    return GENERATION_FOREGROUND_START_MODE
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    controller.timeout()
    dispatchGate.timeout()
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  private fun apply(command: GenerationForegroundCommand, startId: Int) {
    when (command) {
      GenerationForegroundCommand.START -> {
        createNotificationChannel()
        startForeground(
          GENERATION_FOREGROUND_NOTIFICATION_ID,
          androidx.core.app.NotificationCompat.Builder(this, GENERATION_FOREGROUND_CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setContentTitle(getString(R.string.generation_notification_title))
            .setContentText(getString(R.string.generation_notification_text))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build(),
        )
      }
      GenerationForegroundCommand.STOP -> {
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelfResult(startId)
      }
      GenerationForegroundCommand.NONE -> Unit
    }
  }

  private fun createNotificationChannel() {
    getSystemService(NotificationManager::class.java).createNotificationChannel(
      NotificationChannel(
        GENERATION_FOREGROUND_CHANNEL,
        getString(R.string.generation_notification_channel),
        NotificationManager.IMPORTANCE_LOW,
      ),
    )
  }

  companion object {
    private val dispatchGate = GenerationForegroundDispatchGate()

    internal fun start(context: Context): Boolean = dispatchGate.begin {
      runCatching {
        ContextCompat.startForegroundService(
          context,
          Intent(context, GenerationForegroundService::class.java).setAction(GENERATION_FOREGROUND_BEGIN),
        )
        true
      }.getOrDefault(false)
    }

    internal fun stop(context: Context): Boolean = dispatchGate.end {
      runCatching {
        context.startService(
          Intent(context, GenerationForegroundService::class.java).setAction(GENERATION_FOREGROUND_END),
        ) != null
      }.getOrDefault(false)
    }

    internal fun notificationsEnabled(context: Context): Boolean {
      val manager = NotificationManagerCompat.from(context)
      if (!manager.areNotificationsEnabled()) return false
      val channel = manager.getNotificationChannel(GENERATION_FOREGROUND_CHANNEL)
      return channel == null || channel.importance != NotificationManager.IMPORTANCE_NONE
    }
  }
}
