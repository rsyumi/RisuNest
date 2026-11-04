package io.github.rsyumi.risunest

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.annotation.Keep
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

internal const val GENERATION_FOREGROUND_START_MODE = Service.START_NOT_STICKY
private const val GENERATION_FOREGROUND_CHANNEL = "risunest-generation"
private const val GENERATION_FOREGROUND_NOTIFICATION_ID = 0x52474e31
private const val GENERATION_FOREGROUND_BEGIN = "io.github.rsyumi.risunest.GENERATION_FOREGROUND_BEGIN"
private const val GENERATION_FOREGROUND_TOKEN = "io.github.rsyumi.risunest.GENERATION_FOREGROUND_TOKEN"
private const val INVALID_GENERATION_FOREGROUND_TOKEN = -1L
private const val TAG = "RisuNestBackground"

internal const val BACKGROUND_ACTIVE = "active"
internal const val BACKGROUND_DEFERRED = "deferred"
internal const val BACKGROUND_REFUSED = "background"
internal const val BACKGROUND_FAILED = "failed"
internal const val BACKGROUND_INVALID = "invalid"
private val BACKGROUND_TASK_KINDS = setOf("backup", "restore", "sync", "import", "export", "maintenance", "generation")

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

  /** Keeps a service that already runs from stopping, without starting one; returns the token [leave] takes, or null. */
  @Synchronized
  fun join(): Long? {
    if (count == 0) return null
    count += 1
    return activeToken
  }

  /** Ends a [join], unless the service it joined has stopped since. */
  @Synchronized
  fun leave(token: Long, stopService: (Boolean) -> Boolean): Boolean {
    if (token != activeToken) return false
    return end(stopService)
  }

  /** [stopService] receives whether the current start reached the service; one that has not stops itself on arrival. */
  @Synchronized
  fun end(stopService: (Boolean) -> Boolean): Boolean {
    if (count == 0) return false
    count -= 1
    if (count > 0) return true

    val activated = activeStartId != null
    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    return stopService(activated)
  }

  @Synchronized
  fun stopAll(stopService: (Boolean) -> Boolean): Boolean {
    if (count == 0) return false
    val activated = activeStartId != null
    count = 0
    activeToken = INVALID_GENERATION_FOREGROUND_TOKEN
    activeStartId = null
    return stopService(activated)
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

internal data class BackgroundTaskStatus(val kind: String, val percent: Int = -1, val userInitiated: Boolean = false)

/**
 * Holds owned by the page. A deferred hold takes service time only while promoted, which is while the app is off screen.
 * [onHeldChanged] hears when the page starts or stops holding any work, deferred or not, protected or not; it hears
 * that the last hold ended before that hold's service time ends, so it can keep the service from stopping.
 */
internal class BackgroundTaskRegistry(private val onHeldChanged: (Boolean) -> Unit = {}) {
  private class Entry(val kind: String, val deferred: Boolean, val userInitiated: Boolean, val onExpired: (String) -> Unit) {
    var percent = -1
    var holding = false
  }

  private val tasks = linkedMapOf<String, Entry>()
  private var visible = true

  val held: Boolean get() = tasks.isNotEmpty()

  private fun reportHeld(before: Boolean) {
    if (held != before) runCatching { onHeldChanged(held) }
  }

  /** [start] starts or joins the service and returns null, or the reason the platform refused it. */
  fun begin(
    id: String,
    kind: String,
    deferred: Boolean,
    userInitiated: Boolean,
    start: () -> String?,
    onExpired: (String) -> Unit,
    refresh: () -> Unit = {},
  ): String {
    if (kind !in BACKGROUND_TASK_KINDS || !isCanonicalUuidV4(id) || id in tasks) return BACKGROUND_INVALID
    val entry = Entry(kind, deferred, userInitiated, onExpired)
    tasks[id] = entry
    reportHeld(before = tasks.size > 1)
    if (deferred && visible) return BACKGROUND_DEFERRED
    // A refused hold stays registered, so the next time the app leaves the screen protects it.
    start()?.let { return it }
    entry.holding = true
    // Holds a failed start left unprotected join the service this hold started again.
    for (other in tasks.values.filter { !it.holding && (!it.deferred || !visible) }) {
      if (start() != null) break
      other.holding = true
    }
    runCatching(refresh)
    return BACKGROUND_ACTIVE
  }

  fun progress(id: String, percent: Int): Boolean {
    val entry = tasks[id] ?: return false
    if (percent !in -1..100) return false
    entry.percent = percent
    return entry.holding
  }

  fun end(id: String, stop: () -> Boolean): Boolean {
    val entry = tasks.remove(id) ?: return false
    reportHeld(before = true)
    if (entry.holding) stop()
    return true
  }

  /** The app left the screen: every hold the service does not protect yet joins one start. */
  fun promote(start: () -> String?, refresh: () -> Unit = {}) {
    visible = false
    var promoted = false
    for (entry in tasks.values.filter { !it.holding }) {
      if (start() != null) break
      entry.holding = true
      promoted = true
    }
    if (promoted) runCatching(refresh)
  }

  /** The app returned to the screen: deferred holds give their service time back. */
  fun demote(stop: () -> Boolean) {
    visible = true
    val promoted = tasks.values.filter { it.deferred && it.holding }
    promoted.forEach { it.holding = false }
    promoted.forEach { stop() }
  }

  /** The service could not reach the foreground. Holds stay registered, and the next start protects them again. */
  fun release() {
    tasks.values.forEach { it.holding = false }
  }

  fun snapshot(): List<BackgroundTaskStatus> =
    tasks.values.filter { it.holding }.map { BackgroundTaskStatus(it.kind, it.percent, it.userInitiated) }

  /** Ends the holds the service protected, as when its time runs out or it is destroyed. */
  fun clear(notify: Boolean = false) {
    val before = held
    val ended = tasks.filterValues { it.holding }
    ended.keys.forEach(tasks::remove)
    if (notify) ended.forEach { (id, entry) -> runCatching { entry.onExpired(id) } }
    reportHeld(before)
  }

  fun clearAll() {
    val before = held
    tasks.clear()
    reportHeld(before)
  }
}

private const val PAGE_KEPT_ACTIVE_RELEASE_DELAY_MILLIS = 2_000L
// A few seconds past the release, so holds that hidden handlers begin join the running service.
private const val PAGE_KEPT_SERVICE_LINGER_MILLIS = 5_000L

/**
 * Decides when the WebView keeps the page running off screen, so Chromium does not freeze background work.
 * It starts only while the activity is resumed, so a hold the page begins after it went hidden off screen cannot
 * make it visible again, and it ends a moment after the last hold, so work that follows at once stays covered.
 * Off screen, the service stays up from the last hold until a moment after the release, since the app cannot start
 * it again from the background; [keepService] returns what lets it stop, or null when no service runs.
 */
internal class PageKeptActivePolicy(
  private val apply: (Boolean) -> Unit,
  private val keepService: () -> (() -> Unit)?,
  private val postDelayed: (Runnable, Long) -> Unit,
  private val removeCallbacks: (Runnable) -> Unit,
  private val releaseDelayMillis: Long = PAGE_KEPT_ACTIVE_RELEASE_DELAY_MILLIS,
  private val serviceLingerMillis: Long = PAGE_KEPT_SERVICE_LINGER_MILLIS,
) {
  private var held = false
  private var resumed = false
  private var kept = false
  private var serviceKept: (() -> Unit)? = null
  private val letServiceStop = Runnable {
    serviceKept?.invoke()
    serviceKept = null
  }
  private val release = Runnable {
    if (!held) keep(false)
    postDelayed(letServiceStop, serviceLingerMillis)
  }

  fun onHeldChanged(held: Boolean) {
    this.held = held
    update()
  }

  fun onResumedChanged(resumed: Boolean) {
    this.resumed = resumed
    update()
  }

  private fun update() {
    removeCallbacks(release)
    if (resumed) {
      removeCallbacks(letServiceStop)
      letServiceStop.run()
    }
    if (held) {
      if (resumed) keep(true)
    } else if (kept) {
      if (!resumed && serviceKept == null) serviceKept = keepService()
      postDelayed(release, releaseDelayMillis)
    }
  }

  private fun keep(value: Boolean) {
    if (kept == value) return
    kept = value
    apply(value)
  }
}

private fun startNotAllowed(error: Exception): Boolean =
  error.javaClass.name == "android.app.ForegroundServiceStartNotAllowedException"

/** Writes a refused service start to the app log the user can export. */
@Keep
internal object BackgroundServiceLog {
  external fun recordRefused(foreground: Boolean, background: Boolean)

  fun refused(foreground: Boolean, error: Exception) {
    // JVM tests run without the native library.
    runCatching { recordRefused(foreground, startNotAllowed(error)) }
  }
}

internal enum class BackgroundNotificationIcon { SYNC, UPLOAD, DOWNLOAD, CHAT }

internal data class BackgroundNotificationLine(val kind: String, val percent: Int)

internal data class BackgroundNotificationContent(
  val generation: Boolean,
  val lines: List<BackgroundNotificationLine>,
  val percent: Int,
  val icon: BackgroundNotificationIcon,
  val immediate: Boolean,
)

private fun meanPercent(tasks: List<BackgroundTaskStatus>): Int =
  if (tasks.isEmpty() || tasks.any { it.percent < 0 }) -1 else tasks.sumOf { it.percent } / tasks.size

private fun backgroundTaskIcon(kind: String): BackgroundNotificationIcon = when (kind) {
  "backup", "export" -> BackgroundNotificationIcon.UPLOAD
  "restore", "import" -> BackgroundNotificationIcon.DOWNLOAD
  else -> BackgroundNotificationIcon.SYNC
}

/**
 * One line per kind beside generation; the bar is determinate only when every task reports a percent.
 * Work the user started shows at once instead of after the platform's display delay.
 */
internal fun backgroundNotificationContent(tasks: List<BackgroundTaskStatus>): BackgroundNotificationContent {
  val work = tasks.filter { it.kind != "generation" }
  val icons = work.map { backgroundTaskIcon(it.kind) }.toSet()
  return BackgroundNotificationContent(
    generation = work.size < tasks.size,
    lines = work.groupBy { it.kind }.map { (kind, group) -> BackgroundNotificationLine(kind, meanPercent(group)) },
    percent = meanPercent(work),
    icon = when {
      icons.isEmpty() -> BackgroundNotificationIcon.CHAT
      icons.size == 1 -> icons.single()
      else -> BackgroundNotificationIcon.SYNC
    },
    immediate = work.any { it.userInitiated },
  )
}

class GenerationForegroundService : Service() {
  private var wakeLock: PowerManager.WakeLock? = null
  private var activatedToken = INVALID_GENERATION_FOREGROUND_TOKEN
  private var activatedStartId: Int? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val token = if (intent?.action == GENERATION_FOREGROUND_BEGIN) {
      intent.getLongExtra(GENERATION_FOREGROUND_TOKEN, INVALID_GENERATION_FOREGROUND_TOKEN)
    } else {
      INVALID_GENERATION_FOREGROUND_TOKEN
    }
    var started = true
    val current = lifecycle.activate(
      token = token,
      startId = startId,
      startForeground = {
        activatedToken = token
        activatedStartId = startId
        started = startInForeground()
      },
      // Android requires every startForegroundService start to reach the foreground, even one that only stops.
      stopStaleStart = {
        enterForeground()
        stopSelfResult(startId)
      },
    )
    if (current && !started) {
      if (lifecycle.serviceDestroyed(token, startId)) background.release()
      stopSelfResult(startId)
    }
    return GENERATION_FOREGROUND_START_MODE
  }

  override fun onDestroy() {
    val destroyed = activatedStartId?.let { startId ->
      lifecycle.serviceDestroyed(activatedToken, startId)
    } == true
    if (instance === this) instance = null
    if (destroyed) background.clear(notify = true)
    releaseWakeLock()
    super.onDestroy()
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    lifecycle.timeout(
      token = activatedToken,
      startId = startId,
      stopTimedOut = {
        background.clear(notify = true)
        releaseWakeLock()
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelfResult(startId)
      },
      stopStaleTimeout = { stopSelfResult(startId) },
    )
  }

  private fun enterForeground(): Boolean = try {
    startForeground(GENERATION_FOREGROUND_NOTIFICATION_ID, buildNotification())
    true
  } catch (error: Exception) {
    Log.w(TAG, "Foreground service start was refused", error)
    BackgroundServiceLog.refused(foreground = true, error)
    false
  }

  private fun startInForeground(): Boolean {
    if (!enterForeground()) return false
    instance = this
    if (wakeLock == null) {
      wakeLock = (getSystemService(POWER_SERVICE) as PowerManager)
        .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "RisuNest:background-work").apply {
          setReferenceCounted(false)
          acquire(6 * 60 * 60 * 1000L)
        }
    }
    return true
  }

  private fun refreshNotification() {
    // Kept up past its last hold, the service goes on showing the work it last ran.
    if (background.snapshot().isEmpty()) return
    val notification = buildNotification()
    NotificationManagerCompat.from(this).notify(GENERATION_FOREGROUND_NOTIFICATION_ID, notification)
  }

  private fun buildNotification(): android.app.Notification {
    createNotificationChannel()
    val openAppIntent = PendingIntent.getActivity(
      this,
      GENERATION_FOREGROUND_NOTIFICATION_ID,
      Intent(this, MainActivity::class.java).addFlags(
        Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP,
      ),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val content = backgroundNotificationContent(background.snapshot())
    val builder = androidx.core.app.NotificationCompat.Builder(this, GENERATION_FOREGROUND_CHANNEL)
        .setSmallIcon(when (content.icon) {
          BackgroundNotificationIcon.SYNC -> android.R.drawable.stat_notify_sync
          BackgroundNotificationIcon.UPLOAD -> android.R.drawable.stat_sys_upload
          BackgroundNotificationIcon.DOWNLOAD -> android.R.drawable.stat_sys_download
          BackgroundNotificationIcon.CHAT -> android.R.drawable.stat_notify_chat
        })
        .setContentTitle(getString(R.string.generation_notification_title))
        .setContentText(getString(R.string.generation_notification_text))
        .setContentIntent(openAppIntent)
        .setOngoing(true)
        .setOnlyAlertOnce(true)
    if (content.immediate) {
      builder.setForegroundServiceBehavior(androidx.core.app.NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
    }
    if (content.lines.isNotEmpty()) {
      val generation = if (content.generation) listOf(getString(R.string.generation_notification_title)) else emptyList()
      val lines = generation + content.lines.map { line ->
        val title = getString(when (line.kind) {
          "backup" -> R.string.background_backup
          "restore" -> R.string.background_restore
          "sync" -> R.string.background_sync
          "import" -> R.string.background_import
          "export" -> R.string.background_export
          else -> R.string.background_maintenance
        })
        if (line.percent >= 0) "$title ${line.percent}%" else title
      }
      builder.setContentTitle(getString(R.string.background_notification_title))
        .setContentText(lines.joinToString(" · "))
        .setStyle(androidx.core.app.NotificationCompat.InboxStyle().also { style -> lines.forEach { style.addLine(it) } })
        .setProgress(100, maxOf(0, content.percent), content.percent < 0)
    }
    return builder.build()
  }

  private fun releaseWakeLock() {
    wakeLock?.let { if (it.isHeld) it.release() }
    wakeLock = null
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
    private var heldObserver: ((Boolean) -> Unit)? = null
    private val background = BackgroundTaskRegistry { held -> heldObserver?.invoke(held) }
    private var instance: GenerationForegroundService? = null

    /** [observer] hears at once and on every change whether the page holds background work. */
    internal fun observeHeld(observer: (Boolean) -> Unit) {
      heldObserver = observer
      observer(background.held)
    }

    /** A newer activity may have replaced [observer] already, so only that observer detaches. */
    internal fun stopObservingHeld(observer: (Boolean) -> Unit) {
      if (heldObserver === observer) heldObserver = null
    }

    internal fun beginTask(
      context: Context,
      id: String,
      kind: String,
      deferred: Boolean,
      userInitiated: Boolean,
      expired: (String) -> Unit,
    ): String = background.begin(id, kind, deferred, userInitiated, { start(context) }, expired) { instance?.refreshNotification() }

    internal fun taskProgress(id: String, percent: Int): Boolean {
      val updated = background.progress(id, percent)
      if (updated) runCatching { instance?.refreshNotification() }
      return updated
    }

    internal fun endTask(context: Context, id: String): Boolean = background.end(id) { stop(context) }

    /** Keeps the running service from stopping, without starting one; returns what lets it stop, or null. */
    internal fun keepService(context: Context): (() -> Unit)? {
      val token = lifecycle.join() ?: return null
      return { stop(context, token) }
    }

    /** Off screen, deferred holds take the service; back on screen, they give it back. */
    internal fun setVisible(context: Context, visible: Boolean) {
      if (visible) background.demote { stop(context) }
      else background.promote({ start(context) }) { instance?.refreshNotification() }
    }

    private fun start(context: Context): String? {
      var refusal = BACKGROUND_FAILED
      val started = lifecycle.begin { token ->
        try {
          ContextCompat.startForegroundService(
            context,
            Intent(context, GenerationForegroundService::class.java)
              .setAction(GENERATION_FOREGROUND_BEGIN)
              .putExtra(GENERATION_FOREGROUND_TOKEN, token),
          )
          true
        } catch (error: Exception) {
          if (startNotAllowed(error)) refusal = BACKGROUND_REFUSED
          Log.w(TAG, "Background service start was refused", error)
          BackgroundServiceLog.refused(foreground = false, error)
          false
        }
      }
      return if (started) null else refusal
    }

    private fun stopService(context: Context, activated: Boolean): Boolean =
      !activated || runCatching { context.stopService(Intent(context, GenerationForegroundService::class.java)) }.getOrDefault(false)

    private fun stop(context: Context, joined: Long? = null): Boolean {
      var stopping = false
      val stopLast = { activated: Boolean ->
        stopping = true
        stopService(context, activated)
      }
      val result = if (joined == null) lifecycle.end(stopLast) else lifecycle.leave(joined, stopLast)
      if (!stopping) runCatching { instance?.refreshNotification() }
      return result
    }

    internal fun stopAll(context: Context): Boolean {
      background.clearAll()
      instance?.releaseWakeLock()
      return lifecycle.stopAll { activated -> stopService(context, activated) }
    }

    internal fun notificationsEnabled(context: Context): Boolean {
      val manager = NotificationManagerCompat.from(context)
      if (!manager.areNotificationsEnabled()) return false
      val channel = manager.getNotificationChannelCompat(GENERATION_FOREGROUND_CHANNEL)
      return channel == null || channel.importance != NotificationManagerCompat.IMPORTANCE_NONE
    }
  }
}
