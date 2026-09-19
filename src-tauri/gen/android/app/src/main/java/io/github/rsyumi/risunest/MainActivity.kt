package io.github.rsyumi.risunest

import android.content.ComponentCallbacks2
import android.content.Context
import android.content.ContentResolver
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.provider.OpenableColumns
import android.util.Log
import android.view.ViewGroup
import android.webkit.WebView
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.IntentCompat
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updateLayoutParams
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.security.MessageDigest
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private const val EXIT_CONFIRMATION_WINDOW_MILLIS = 2_000L
private const val EXIT_FLUSH_TIMEOUT_MILLIS = 1_500L
private const val NATIVE_LIFECYCLE_EVENT = "risu-native-lifecycle"
private const val OPENED_FILES_EVENT = "risu-opened-files"
private const val STOP_REASON = "stop"
private const val TRIM_MEMORY_REASON = "trim-memory"
private const val EXIT_REASON = "exit"
// Vite 8's pinned Baseline target starts at Chrome 111. Update this with the web build target.
private const val MINIMUM_WEBVIEW_MAJOR = 111
private const val NATIVE_RESILIENCE_PREFERENCES = "risu-native-resilience"
private const val RENDERER_RECOVERY_MARKER = "renderer-recovery-warning"
private const val SAF_PROGRESS_INTERVAL_MILLIS = 100L
private const val LEGACY_OPENED_FILE_STALE_MILLIS = 24 * 60 * 60 * 1_000L
private const val OPENED_FILE_INTENT_CONSUMED = "io.github.rsyumi.risunest.OPENED_FILE_INTENT_CONSUMED"
private const val OPENED_FILE_FINGERPRINT_STATE = "risu.opened-file-fingerprint"
private const val BACKUP_SOURCE_REQUEST_STATE = "risu.backup-source-request"
private const val BACKUP_SOURCE_CANCELLED_STATE = "risu.backup-source-cancelled"
private const val CONTENT_SOURCE_STATE = "risu.content-source"
private const val LEGACY_BACKUP_SOURCE_REQUEST_STATE = "risu.legacy-backup-source-request"
private const val LEGACY_BACKUP_SOURCE_CANCELLED_STATE = "risu.legacy-backup-source-cancelled"
private const val TAG = "RisuNative"

internal typealias RendererRecoveryFailureLogger = (step: String, error: Throwable) -> Unit

private fun logRendererRecoveryFailure(step: String, error: Throwable) {
  Log.e(TAG, "Renderer recovery step failed: $step", error)
}

internal enum class WebViewProviderStatus {
  SUPPORTED,
  MISSING,
  OUTDATED,
  UNKNOWN_VERSION,
}

internal data class WebViewProviderDecision(
  val status: WebViewProviderStatus,
  val majorVersion: Int?,
)

internal fun decideWebViewProvider(
  packageName: String?,
  versionName: String?,
  minimumMajor: Int = MINIMUM_WEBVIEW_MAJOR,
): WebViewProviderDecision {
  if (packageName.isNullOrBlank()) {
    return WebViewProviderDecision(WebViewProviderStatus.MISSING, null)
  }
  val majorVersion = versionName?.substringBefore('.')?.toIntOrNull()
    ?: return WebViewProviderDecision(WebViewProviderStatus.UNKNOWN_VERSION, null)
  return WebViewProviderDecision(
    status = if (majorVersion >= minimumMajor) {
      WebViewProviderStatus.SUPPORTED
    } else {
      WebViewProviderStatus.OUTDATED
    },
    majorVersion = majorVersion,
  )
}

internal class OneShotRecoveryMarker(
  private val isMarked: () -> Boolean,
  private val setMarked: (Boolean) -> Unit,
) {
  fun mark() {
    setMarked(true)
  }

  fun consume(): Boolean {
    if (!isMarked()) return false
    setMarked(false)
    return true
  }
}

internal class RendererRecoveryCoordinator(
  private val logFailure: RendererRecoveryFailureLogger,
) {
  private var recovering = false

  fun recover(
    removeFromParent: () -> Unit,
    removeJavascriptBridge: () -> Unit,
    destroyView: () -> Unit,
    clearReference: () -> Unit,
    markRecovery: () -> Unit,
    restart: () -> Boolean,
  ): Boolean {
    if (recovering) return true
    recovering = true
    var cleanedUp = true
    listOf(
      "remove-from-parent" to removeFromParent,
      "remove-javascript-bridge" to removeJavascriptBridge,
      "destroy-view" to destroyView,
      "clear-reference" to clearReference,
      "mark-recovery" to markRecovery,
    ).forEach { (name, step) ->
      try {
        step()
      } catch (error: Throwable) {
        if (name != "mark-recovery") cleanedUp = false
        logFailure(name, error)
      }
    }
    return try {
      val restarting = restart()
      cleanedUp && restarting
    } catch (error: Throwable) {
      logFailure("restart", error)
      false
    }
  }
}

interface RendererRecoveryHost {
  fun recoverRenderer(webView: WebView, didCrash: Boolean): Boolean
}

internal data class WebViewMargins(
  val left: Int,
  val top: Int,
  val right: Int,
  val bottom: Int,
)

internal fun resolveWebViewMargins(
  systemBars: WebViewMargins,
  displayCutout: WebViewMargins,
) = WebViewMargins(
  left = maxOf(systemBars.left, displayCutout.left),
  top = maxOf(systemBars.top, displayCutout.top),
  right = maxOf(systemBars.right, displayCutout.right),
  bottom = maxOf(systemBars.bottom, displayCutout.bottom),
)

internal fun nativeMarginInsetTypes() = WindowInsetsCompat.Type.systemBars() or
  WindowInsetsCompat.Type.displayCutout()

internal enum class BackNavigationAction {
  GO_BACK,
  SHOW_EXIT_HINT,
  EXIT,
}

internal class BackNavigationPolicy(
  private val confirmationWindowMillis: Long = EXIT_CONFIRMATION_WINDOW_MILLIS,
) {
  private var lastRootBackPressMillis: Long? = null

  fun decide(canGoBack: Boolean, nowMillis: Long): BackNavigationAction {
    if (canGoBack) {
      lastRootBackPressMillis = null
      return BackNavigationAction.GO_BACK
    }

    val previousPressMillis = lastRootBackPressMillis
    if (previousPressMillis != null && nowMillis - previousPressMillis <= confirmationWindowMillis) {
      lastRootBackPressMillis = null
      return BackNavigationAction.EXIT
    }

    lastRootBackPressMillis = nowMillis
    return BackNavigationAction.SHOW_EXIT_HINT
  }
}

internal fun sanitizeOpenedFileName(name: String): String {
  val leaf = name.substringAfterLast('/').substringAfterLast('\\')
  val safe = leaf.replace(Regex("[^A-Za-z0-9._-]"), "_")
  return safe.ifBlank { "opened-file" }
}

internal fun escapeJsStringLiteral(value: String): String = buildString {
  for (character in value) {
    when {
      character == '\\' -> append("\\\\")
      character == '"' -> append("\\\"")
      character == '\u2028' || character == '\u2029' || character < ' ' ->
        append("\\u%04x".format(character.code))
      else -> append(character)
    }
  }
}

/**
 * Announces files opened while the app was already running.
 *
 * The event is cancelable so the web app can report that it consumed the payload. When nothing
 * listens yet, because the page is still booting, the files fall back to the same startup queue the
 * cold start path fills.
 */
internal fun openedFilesEventScript(paths: List<String>): String {
  val values = paths.joinToString(",") { "\"${escapeJsStringLiteral(it)}\"" }
  return "(function(){var files=[$values];" +
    "var event=new CustomEvent('$OPENED_FILES_EVENT',{detail:{files:files},cancelable:true});" +
    "if(window.dispatchEvent(event)){" +
    "window.tauriOpenedFiles=" +
    "(Array.isArray(window.tauriOpenedFiles)?window.tauriOpenedFiles:[]).concat(files);" +
    "}})();"
}

private data class OpenedFileSource(
  val uri: Uri,
  val displayName: String,
  val totalBytes: Long?,
)

internal class RestoredIntentConsumptionMarker(
  private val isConsumed: () -> Boolean,
  private val markConsumed: () -> Unit,
) {
  fun claim(): Boolean {
    if (isConsumed()) return false
    markConsumed()
    return true
  }
}

internal fun openedFileIntentFingerprint(action: String?, uris: List<String>): String {
  val digest = MessageDigest.getInstance("SHA-256")
  for (value in listOf(action.orEmpty()) + uris) {
    val bytes = value.toByteArray(Charsets.UTF_8)
    digest.update(bytes.size.toString().toByteArray(Charsets.US_ASCII))
    digest.update(':'.code.toByte())
    digest.update(bytes)
  }
  return digest.digest().joinToString("") { "%02x".format(it.toInt() and 0xff) }
}

private data class PendingSafDestination(
  val requestId: String,
  val exportId: String,
  val sourceKind: SafDestinationSourceKind,
  val source: File?,
  val cancellation: AtomicBoolean,
)

private data class PendingBackupSource(
  val requestId: String,
  val cancellation: AtomicBoolean,
  val restored: Boolean = false,
)

private data class PendingLegacyBackupSource(
  val requestId: String,
  val cancellation: AtomicBoolean,
  val content: Boolean = false,
  val restored: Boolean = false,
  val importDestination: SafContentImportDestination? = null,
)

internal class LifecycleFlushDispatcher(
  private val dispatch: (String) -> Unit,
) {
  fun onStop() {
    dispatch(STOP_REASON)
  }

  fun onTrimMemory(level: Int) {
    if (level >= ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN) {
      dispatch(TRIM_MEMORY_REASON)
    }
  }
}

internal class ExitFlushGate {
  private var pendingToken: String? = null

  fun begin(token: String) {
    pendingToken = token
  }

  fun cancel(token: String) {
    if (pendingToken == token) {
      pendingToken = null
    }
  }

  fun shouldFinish(token: String): Boolean {
    if (pendingToken != token) {
      return false
    }
    pendingToken = null
    return true
  }
}

internal class ColdRestartDispatcher(
  private val relaunchTask: () -> Unit,
  private val terminateProcess: () -> Unit,
  private val logFailure: RendererRecoveryFailureLogger,
) {
  fun restart(): Boolean {
    try {
      relaunchTask()
    } catch (error: Throwable) {
      logFailure("relaunch-task", error)
    }
    try {
      terminateProcess()
    } catch (error: Throwable) {
      logFailure("terminate-process", error)
    }
    return false
  }
}

// SAF picker admission runs on the WebView JavaBridge thread, but every WebView
// or ActivityResultLauncher touch must reach the main thread through postToMain.
internal class SafSourcePickFlow(
  private val postToMain: (() -> Unit) -> Unit,
) {
  fun begin(
    acquireSlot: () -> Boolean,
    registerRequest: () -> Boolean,
    releaseSlot: () -> Unit,
    dispatchBusy: () -> Unit,
    startPicker: () -> Unit,
  ) {
    if (!acquireSlot()) {
      postToMain(dispatchBusy)
      return
    }
    if (!registerRequest()) {
      releaseSlot()
      postToMain(dispatchBusy)
      return
    }
    postToMain(startPicker)
  }
}

internal fun launchSafSourcePicker(
  launch: () -> Unit,
  onFailure: () -> Unit,
) {
  try {
    launch()
  } catch (error: Exception) {
    onFailure()
  }
}

internal class SafProgressThrottle(
  private val intervalMillis: Long = SAF_PROGRESS_INTERVAL_MILLIS,
) {
  private val lastDispatchMillis = ConcurrentHashMap<String, Long>()

  fun shouldDispatch(key: String, nowMillis: Long): Boolean {
    var shouldDispatch = false
    lastDispatchMillis.compute(key) { _, previous ->
      if (previous == null || nowMillis - previous >= intervalMillis) {
        shouldDispatch = true
        nowMillis
      } else {
        previous
      }
    }
    return shouldDispatch
  }

  fun clear(requestId: String) {
    lastDispatchMillis.keys.removeAll {
      it == requestId || it.startsWith("$requestId:")
    }
  }

  fun clearAll() {
    lastDispatchMillis.clear()
  }
}

internal fun cleanupLegacyOpenedFiles(
  directory: File,
  nowMillis: Long = System.currentTimeMillis(),
  staleAfterMillis: Long = LEGACY_OPENED_FILE_STALE_MILLIS,
): List<String> {
  if (!directory.isDirectory) return emptyList()
  val cutoff = nowMillis - staleAfterMillis
  return runCatching { directory.listFiles().orEmpty().toList() }.getOrDefault(emptyList())
    .filter {
      it.isFile && it.lastModified() <= cutoff && runCatching(it::delete).getOrDefault(false)
    }
    .map(File::getName)
    .sorted()
}

internal fun androidSafDestinationScriptForRecord(
  record: SafDestinationRecord,
  message: String?,
) = androidSafDestinationScript(
  requestId = record.requestId,
  exportId = record.exportId,
  sourceKind = record.sourceKind,
  state = record.phase.wireName,
  bytes = record.bytes,
  code = record.code,
  message = message,
  warningCodes = record.warningCodes,
  publicationPrerequisitesComplete = record.publicationPrerequisitesComplete,
)

internal class PostNotificationsRequestGate(
  private val sdkInt: Int,
  private val requestedInProcess: AtomicBoolean,
) {
  fun shouldRequest(isGranted: () -> Boolean): Boolean {
    if (sdkInt < 33 || isGranted()) return false
    return requestedInProcess.compareAndSet(false, true)
  }
}

internal fun requestPostNotificationsIfNeeded(
  gate: PostNotificationsRequestGate,
  isGranted: () -> Boolean,
  postToMain: (() -> Unit) -> Unit,
  launchRequest: () -> Unit,
) {
  if (!gate.shouldRequest(isGranted)) return
  postToMain(launchRequest)
}

internal fun beginGenerationKeepAlive(
  notificationsEnabled: () -> Boolean,
  startService: () -> Boolean,
): Boolean {
  if (!notificationsEnabled()) return false
  return startService()
}

internal class GenerationKeepAliveOwner {
  private var closed = false

  @Synchronized
  fun begin(startGeneration: () -> Boolean): Boolean {
    if (closed) return false
    return startGeneration()
  }

  @Synchronized
  fun end(stopGeneration: () -> Boolean): Boolean {
    if (closed) return false
    return stopGeneration()
  }

  @Synchronized
  fun teardown(
    removeJavascriptBridge: () -> Unit,
    stopGeneration: () -> Unit,
  ) {
    if (closed) return
    closed = true
    try {
      removeJavascriptBridge()
    } finally {
      stopGeneration()
    }
  }
}

private val postNotificationsRequestedInProcess = AtomicBoolean(false)

class MainActivity : TauriActivity(), RendererRecoveryHost {
  private val backNavigationPolicy = BackNavigationPolicy()
  private var lifecycleWebView: WebView? = null
  private var frontendReady = AndroidFrontendReady()
  private var commitBridge: AndroidCommitBridge? = null
  private var controlBridge: AndroidControlBridge? = null
  private var safControlBridge: AndroidControlBridge? = null
  private val lifecycleCommands = LifecycleFlushBridge()
  private val generationCommands = GenerationKeepAliveBridge()
  private val safCommands = SafBridge()
  private val lifecycleFlushDispatcher = LifecycleFlushDispatcher(::dispatchLifecycleFlush)
  private val exitFlushGate = ExitFlushGate()
  private val rendererRecoveryCoordinator = RendererRecoveryCoordinator(::logRendererRecoveryFailure)
  private val rendererRestartGate by lazy {
    RendererRestartGate(
      post = { work -> mainHandler.post { work() } },
      restart = { if (!coldRestartDispatcher.restart()) finishAndRemoveTask() },
    )
  }
  private val generationKeepAliveOwner = GenerationKeepAliveOwner()
  private val mainHandler = Handler(Looper.getMainLooper())
  private val safScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
  private val safSourcePickFlow = SafSourcePickFlow { block -> safScope.launch { block() } }
  private val postNotificationsGate = PostNotificationsRequestGate(
    Build.VERSION.SDK_INT,
    postNotificationsRequestedInProcess,
  )
  private val safSourceCancellations = ConcurrentHashMap<String, AtomicBoolean>()
  private val safDestinationCancellations = ConcurrentHashMap<String, AtomicBoolean>()
  private val safProgressThrottle = SafProgressThrottle()
  private val deliveredSpoolTokens = ConcurrentHashMap.newKeySet<String>()
  private var pendingSafDestination: PendingSafDestination? = null
  private var pendingLegacyBackupSource: PendingLegacyBackupSource? = null
  private val safPickerSlot = SafDestinationSlot()
  private val safDestinationStateLock = Any()
  private var consumedOpenedFileFingerprint: String? = null
  private var pendingBackupSource: PendingBackupSource? = null
  private val safDestinationStateStore by lazy {
    SafDestinationStateStore(
      File(dataDir, "native-file-jobs/android-saf-destination.json"),
      AndroidSafAtomicPublisher,
    )
  }
  private val safDestinationPicker = registerForActivityResult(
    object : ActivityResultContracts.CreateDocument("application/octet-stream") {
      override fun createIntent(context: Context, input: String): Intent =
        super.createIntent(context, input).setType(androidExportMimeType(input))
    },
    ::onSafDestinationSelected,
  )
  private val backupSourcePicker = registerForActivityResult(
    ActivityResultContracts.OpenDocument(),
    ::onBackupSourceSelected,
  )
  private val legacyBackupSourcePicker = registerForActivityResult(
    ActivityResultContracts.OpenDocument(),
    ::onLegacyBackupSourceSelected,
  )
  private val postNotificationsPermissionLauncher = registerForActivityResult(
    ActivityResultContracts.RequestPermission(),
  ) {
    // Generation keep-alive checks notification
    // availability on each begin and declines to start until it is granted.
    dispatchNotificationStateRefresh()
  }
  private val rendererRecoveryMarker by lazy {
    val preferences = getSharedPreferences(NATIVE_RESILIENCE_PREFERENCES, MODE_PRIVATE)
    OneShotRecoveryMarker(
      isMarked = { preferences.getBoolean(RENDERER_RECOVERY_MARKER, false) },
      setMarked = { marked ->
        val editor = preferences.edit()
        if (marked) {
          editor.putBoolean(RENDERER_RECOVERY_MARKER, true)
        } else {
          editor.remove(RENDERER_RECOVERY_MARKER)
        }
        editor.commit()
      },
    )
  }
  private val coldRestartDispatcher by lazy {
    ColdRestartDispatcher(
      relaunchTask = {
        startActivity(Intent.makeRestartActivityTask(componentName))
      },
      terminateProcess = {
        android.os.Process.killProcess(android.os.Process.myPid())
      },
      logFailure = ::logRendererRecoveryFailure,
    )
  }
  private var exitFlushSequence = 0L

  override fun onCreate(savedInstanceState: Bundle?) {
    consumedOpenedFileFingerprint = savedInstanceState?.getString(OPENED_FILE_FINGERPRINT_STATE)
    savedInstanceState?.let { state ->
      state.getString(BACKUP_SOURCE_REQUEST_STATE)
        ?.takeIf(::isCanonicalUuidV4)
        ?.let { requestId ->
          val cancellation = AtomicBoolean(
            state.getBoolean(BACKUP_SOURCE_CANCELLED_STATE, false),
          )
          pendingBackupSource = PendingBackupSource(requestId, cancellation, restored = true)
          safSourceCancellations[requestId] = cancellation
          safPickerSlot.acquireRestored()
        }
      state.getString(LEGACY_BACKUP_SOURCE_REQUEST_STATE)
        ?.takeIf(::isCanonicalUuidV4)
        ?.let { requestId ->
          val cancellation = AtomicBoolean(
            state.getBoolean(LEGACY_BACKUP_SOURCE_CANCELLED_STATE, false) || state.getBoolean(CONTENT_SOURCE_STATE, false),
          )
          pendingLegacyBackupSource = PendingLegacyBackupSource(
            requestId,
            cancellation,
            restored = true,
            content = state.getBoolean(CONTENT_SOURCE_STATE, false),
          )
          safSourceCancellations[requestId] = cancellation
          safPickerSlot.acquireRestored()
        }
    }
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    safScope.launch(Dispatchers.IO) {
      cleanupLegacyOpenedFiles(File(cacheDir, "opened_files"))
    }
    ServerSyncSecrets.initialize()
    ExternalStorageSecrets.initialize()
    ExternalStorageAuthorization.onOAuthRedirectIntent(intent)
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      recoverSafDestination(savedInstanceState != null)
    }
    showRendererRecoveryWarning()
    diagnoseWebViewProvider()
  }

  override fun recoverRenderer(webView: WebView, didCrash: Boolean): Boolean {
    frontendReady.cancel()
    commitBridge?.close()
    commitBridge = null
    Log.e(TAG, "Android WebView renderer exited, didCrash=$didCrash")
    return rendererRecoveryCoordinator.recover(
      removeFromParent = { (webView.parent as? ViewGroup)?.removeView(webView) },
      removeJavascriptBridge = {
        controlBridge?.close()
        controlBridge = null
        safControlBridge?.close()
        safControlBridge = null
      },
      destroyView = webView::destroy,
      clearReference = {
        if (lifecycleWebView === webView) {
          lifecycleWebView = null
        }
        releaseFailedWebView(webView)
      },
      markRecovery = rendererRecoveryMarker::mark,
      restart = rendererRestartGate::request,
    )
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    frontendReady.cancel()
    frontendReady = AndroidFrontendReady()
    lifecycleWebView = webView
    commitBridge?.close()
    commitBridge = AndroidCommitBridge.attach(webView)
    controlBridge?.close()
    controlBridge = AndroidControlBridge.attach(webView, ::dispatchControl)
    safControlBridge?.close()
    safControlBridge = null
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      safControlBridge = AndroidControlBridge.attach(webView, ::dispatchControl, "RisuNestSafControl")
      deliveredSpoolTokens.clear()
      injectOpenedFiles(webView)
    } else {
      stageLegacyOpenedFiles(webView, claimOpenedFileUris(intent))
    }

    val contentRoot = findViewById<ViewGroup>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(contentRoot) { _, windowInsets ->
      val handledInsetTypes = nativeMarginInsetTypes()
      val systemBars = windowInsets.getInsets(WindowInsetsCompat.Type.systemBars())
      val displayCutout = windowInsets.getInsets(WindowInsetsCompat.Type.displayCutout())
      val margins = resolveWebViewMargins(
        systemBars = WebViewMargins(systemBars.left, systemBars.top, systemBars.right, systemBars.bottom),
        displayCutout = WebViewMargins(
          displayCutout.left,
          displayCutout.top,
          displayCutout.right,
          displayCutout.bottom,
        ),
      )
      webView.updateLayoutParams<ViewGroup.MarginLayoutParams> {
        leftMargin = margins.left
        topMargin = margins.top
        rightMargin = margins.right
        bottomMargin = margins.bottom
      }
      WindowInsetsCompat.Builder(windowInsets)
        .setInsets(handledInsetTypes, Insets.NONE)
        .build()
    }

    onBackPressedDispatcher.addCallback(
      this,
      object : OnBackPressedCallback(true) {
        override fun handleOnBackPressed() {
          if (lifecycleWebView !== webView) return
          when (backNavigationPolicy.decide(webView.canGoBack(), SystemClock.elapsedRealtime())) {
            BackNavigationAction.GO_BACK -> webView.goBack()
            BackNavigationAction.SHOW_EXIT_HINT -> {
              dispatchLifecycleFlush(EXIT_REASON)
              Toast.makeText(
                this@MainActivity,
                R.string.press_back_again_to_exit,
                Toast.LENGTH_SHORT,
              ).show()
            }
            BackNavigationAction.EXIT -> requestExitFlushThenFinish()
          }
        }
      },
    )
  }


  override fun onStop() {
    lifecycleFlushDispatcher.onStop()
    super.onStop()
  }

  override fun onResume() {
    super.onResume()
    rendererRestartGate.onResume()
    dispatchNotificationStateRefresh()
  }

  override fun onPause() {
    rendererRestartGate.onPause()
    super.onPause()
  }

  private fun dispatchNotificationStateRefresh() {
    lifecycleWebView?.evaluateJavascript(
      "window.dispatchEvent(new Event('risunest-android-notifications-changed'));",
      null,
    )
  }

  override fun onSaveInstanceState(outState: Bundle) {
    consumedOpenedFileFingerprint?.let {
      outState.putString(OPENED_FILE_FINGERPRINT_STATE, it)
    }
    pendingBackupSource?.let { pending ->
      outState.putString(BACKUP_SOURCE_REQUEST_STATE, pending.requestId)
      outState.putBoolean(BACKUP_SOURCE_CANCELLED_STATE, pending.cancellation.get())
    }
    pendingLegacyBackupSource?.let { pending ->
      outState.putBoolean(CONTENT_SOURCE_STATE, pending.content)
      outState.putString(LEGACY_BACKUP_SOURCE_REQUEST_STATE, pending.requestId)
      outState.putBoolean(LEGACY_BACKUP_SOURCE_CANCELLED_STATE, pending.cancellation.get())
    }
    super.onSaveInstanceState(outState)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    if (ExternalStorageAuthorization.onOAuthRedirectIntent(intent)) {
      setIntent(intent)
      return
    }
    setIntent(intent)
    consumedOpenedFileFingerprint = null
    lifecycleWebView?.let {
      if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) injectOpenedFiles(it, intent)
      else stageLegacyOpenedFiles(it, claimOpenedFileUris(intent))
    }
  }

  override fun onDestroy() {
    frontendReady.cancel()
    rendererRestartGate.close()
    commitBridge?.close()
    commitBridge = null
    safSourceCancellations.values.forEach { it.set(true) }
    safDestinationCancellations.values.forEach { it.set(true) }
    pendingSafDestination = null
    pendingBackupSource = null
    pendingLegacyBackupSource = null
    safScope.cancel()
    mainHandler.removeCallbacksAndMessages(null)
    safSourceCancellations.clear()
    safDestinationCancellations.clear()
    safProgressThrottle.clearAll()
    generationKeepAliveOwner.teardown(
      removeJavascriptBridge = {
        controlBridge?.close()
        controlBridge = null
        safControlBridge?.close()
        safControlBridge = null
      },
      stopGeneration = { GenerationForegroundService.stopAll(this) },
    )
    lifecycleWebView = null
    super.onDestroy()
  }

  override fun onTrimMemory(level: Int) {
    lifecycleFlushDispatcher.onTrimMemory(level)
    super.onTrimMemory(level)
  }

  private fun dispatchLifecycleFlush(reason: String) {
    lifecycleWebView?.evaluateJavascript(
      "window.dispatchEvent(new CustomEvent('$NATIVE_LIFECYCLE_EVENT',{detail:{reason:'$reason'}}));",
      null,
    )
  }

  private fun requestExitFlushThenFinish() {
    val webView = lifecycleWebView
    if (webView == null) {
      finishAndRemoveTask()
      return
    }
    val token = "exit-${++exitFlushSequence}"
    exitFlushGate.begin(token)
    webView.evaluateJavascript(
      "window.dispatchEvent(new CustomEvent('$NATIVE_LIFECYCLE_EVENT'," +
        "{detail:{reason:'$EXIT_REASON',ackToken:'$token'}}));",
      null,
    )
    mainHandler.postDelayed({ finishForExitFlush(token) }, EXIT_FLUSH_TIMEOUT_MILLIS)
  }

  private fun finishForExitFlush(token: String) {
    if (exitFlushGate.shouldFinish(token)) {
      finishAndRemoveTask()
    }
  }

  private fun onFrontendReady() {
    val webView = lifecycleWebView ?: return
    if (!frontendReady.markReady()) return
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      replayReadySpools(webView)
      replaySafDestinationResult(webView)
    }
  }

  private suspend fun dispatchControl(method: String, args: List<String>): Any? {
    if (method.startsWith("saf.") && !BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      error("android-saf-unavailable")
    }
    return when (method) {
      "lifecycle.onFrontendReady" -> onFrontendReady()
      "lifecycle.onFlushComplete" -> lifecycleCommands.onFlushComplete(args[0])
      "lifecycle.onFlushHold" -> lifecycleCommands.onFlushHold(args[0])
      "lifecycle.requestExit" -> lifecycleCommands.requestExit()
      "lifecycle.requestRestart" -> lifecycleCommands.requestRestart()
      "generation.begin" -> generationCommands.begin()
      "generation.end" -> generationCommands.end()
      "generation.notificationsEnabled" -> generationCommands.notificationsEnabled()
      "generation.requestNotifications" -> requestPostNotificationsForForegroundService()
      "generation.openNotificationSettings" -> generationCommands.openNotificationSettings()
      "generation.webViewVersion" -> generationCommands.webViewVersion()
      "saf.pickBackupSource" -> safCommands.pickBackupSource(args[0])
      "saf.pickLegacyBackupSource" -> safCommands.pickLegacyBackupSource(args[0])
      "saf.pickContentSource" -> safCommands.pickContentSource(args[0], args[1])
      "saf.copyExport" -> safCommands.copyExport(args[0], args[1], args[2])
      "saf.cancelSource" -> safCommands.cancelSource(args[0])
      else -> withContext(Dispatchers.IO) {
        when (method) {
          "saf.cancelExport" -> safCommands.cancelExport(args[0])
          "saf.discardSource" -> safCommands.discardSource(args[0])
          "saf.getActiveSourceRequestIds" -> safCommands.getActiveSourceRequestIds()
          "saf.getExportStatus" -> safCommands.getExportStatus()
          "saf.getExportSourceId" -> safCommands.getExportSourceId()
          "saf.markExportPublicationReady" -> safCommands.markExportPublicationReady(args[0])
          "saf.acknowledgeExport" -> safCommands.acknowledgeExport(args[0])
          else -> error("android-control-unavailable")
        }
      }
    }
  }

  private inner class LifecycleFlushBridge {
    fun onFlushComplete(token: String?) {
      token ?: return
      mainHandler.post { finishForExitFlush(token) }
    }

    fun onFlushHold(token: String?) {
      token ?: return
      mainHandler.post { exitFlushGate.cancel(token) }
    }

    fun requestExit() {
      mainHandler.post { finishAndRemoveTask() }
    }

    fun requestRestart() {
      mainHandler.post { coldRestartDispatcher.restart() }
    }
  }

  private inner class GenerationKeepAliveBridge {
    fun begin(): Boolean = generationKeepAliveOwner.begin {
      beginGenerationKeepAlive(
        notificationsEnabled = { GenerationForegroundService.notificationsEnabled(this@MainActivity) },
        startService = { GenerationForegroundService.start(this@MainActivity) },
      )
    }

    fun end(): Boolean = generationKeepAliveOwner.end {
      GenerationForegroundService.stop(this@MainActivity)
    }

    fun notificationsEnabled(): Boolean = GenerationForegroundService.notificationsEnabled(this@MainActivity)

    fun openNotificationSettings(): Boolean = runCatching {
      val settingsIntent = if (Build.VERSION.SDK_INT >= 26) {
        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
          .putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
      } else {
        Intent(
          Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
          Uri.fromParts("package", packageName, null),
        )
      }
      startActivity(settingsIntent)
      true
    }.getOrDefault(false)

    fun webViewVersion(): String = WebViewCompat.getCurrentWebViewPackage(this@MainActivity)?.versionName ?: ""

  }


  // Android 13+ can hide foreground-service notifications unless POST_NOTIFICATIONS
  // is granted. Request it once per process. Generation keep-alive
  // declines to start until notifications and its channel are available.
  private fun requestPostNotificationsForForegroundService() {
    requestPostNotificationsIfNeeded(
      gate = postNotificationsGate,
      isGranted = {
        checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) ==
          android.content.pm.PackageManager.PERMISSION_GRANTED
      },
      postToMain = { block -> mainHandler.post { block() } },
      launchRequest = {
        runCatching {
          postNotificationsPermissionLauncher.launch(
            android.Manifest.permission.POST_NOTIFICATIONS,
          )
        }
      },
    )
  }

  private inner class SafBridge {
    fun pickBackupSource(requestId: String) {
      if (!isCanonicalUuidV4(requestId)) return
      val cancellation = AtomicBoolean(false)
      safSourcePickFlow.begin(
        acquireSlot = safPickerSlot::tryAcquire,
        registerRequest = { safSourceCancellations.putIfAbsent(requestId, cancellation) == null },
        releaseSlot = safPickerSlot::release,
        dispatchBusy = {
          dispatchBackupSourceBatch(
            requestId,
            SafSpoolBatch(
              emptyList(),
              listOf(SafSpoolFailure("backup.risunest", "source-busy")),
            ),
          )
        },
        startPicker = {
          pendingBackupSource = PendingBackupSource(requestId, cancellation)
          launchSafSourcePicker(
            launch = { backupSourcePicker.launch(arrayOf("*/*")) },
            onFailure = {
              pendingBackupSource = null
              safSourceCancellations.remove(requestId, cancellation)
              safPickerSlot.release()
              dispatchBackupSourceBatch(
                requestId,
                SafSpoolBatch(
                  emptyList(),
                  listOf(SafSpoolFailure("backup.risunest", "source-picker-failed")),
                ),
              )
            },
          )
        },
      )
    }

    fun pickLegacyBackupSource(requestId: String) = pickDocumentSource(requestId, false, null)

    fun pickContentSource(requestId: String, destination: String) {
      if (!isCanonicalUuidV4(requestId)) return
      val importDestination = SafContentImportDestination.fromWireName(destination)
      if (importDestination == null) {
        dispatchDocumentSourceBatch(
          true,
          requestId,
          SafSpoolBatch(
            emptyList(),
            listOf(SafSpoolFailure("content", "invalid-import-destination")),
          ),
        )
        return
      }
      pickDocumentSource(requestId, true, importDestination)
    }

    private fun pickDocumentSource(
      requestId: String,
      content: Boolean,
      importDestination: SafContentImportDestination?,
    ) {
      if (!isCanonicalUuidV4(requestId)) return
      val cancellation = AtomicBoolean(false)
      safSourcePickFlow.begin(
        acquireSlot = safPickerSlot::tryAcquire,
        registerRequest = { safSourceCancellations.putIfAbsent(requestId, cancellation) == null },
        releaseSlot = safPickerSlot::release,
        dispatchBusy = {
          dispatchDocumentSourceBatch(
            content,
            requestId,
            SafSpoolBatch(emptyList(), listOf(SafSpoolFailure("backup.bin", "source-busy"))),
          )
        },
        startPicker = {
          pendingLegacyBackupSource = PendingLegacyBackupSource(
            requestId,
            cancellation,
            content = content,
            importDestination = importDestination,
          )
          launchSafSourcePicker(
            launch = { legacyBackupSourcePicker.launch(arrayOf("*/*")) },
            onFailure = {
              pendingLegacyBackupSource = null
              safSourceCancellations.remove(requestId, cancellation)
              safPickerSlot.release()
              dispatchDocumentSourceBatch(
                content,
                requestId,
                SafSpoolBatch(
                  emptyList(),
                  listOf(SafSpoolFailure("backup.bin", "source-picker-failed")),
                ),
              )
            },
          )
        },
      )
    }

    fun copyExport(
      requestId: String,
      sourcePath: String,
      suggestedName: String,
    ) {
      if (!isCanonicalUuidV4(requestId)) return
      if (!safPickerSlot.tryAcquire()) {
        safScope.launch {
          val busy = destinationTerminalRecord(
            requestId = requestId,
            exportId = requestId,
            phase = SafDestinationPhase.FAILED,
            code = "destination-busy",
            warningCodes = emptyList(),
          )
          dispatchSafDestination(busy, "Another Android SAF destination picker is already open")
        }
        return
      }
      val cancellation = AtomicBoolean(false)
      safDestinationCancellations[requestId] = cancellation
      safScope.launch {
        var terminalRecord: SafDestinationRecord? = null
        var terminalMessage: String? = null
        var ownsPersistedState = false
        var sourceKind = SafDestinationSourceKind.RISU_SAVE
        try {
          val source = withContext(Dispatchers.IO) {
            resolveManagedExportSource(dataDir, sourcePath)
          } ?: throw SafDestinationException(
            "invalid-source",
            emptyList(),
            "Android SAF export source is not an owned native export",
          )
          val exportId = managedExportId(source) ?: throw SafDestinationException(
            "invalid-source",
            emptyList(),
            "Android SAF export source is not an owned native export",
          )
          sourceKind = managedExportSourceKind(source)
          if (cancellation.get()) {
            throw SafDestinationException(
              "cancelled",
              emptyList(),
              "Android SAF destination copy was cancelled",
            )
          }
          val state = SafDestinationRecord(
            requestId = requestId,
            exportId = exportId,
            phase = SafDestinationPhase.PICKING,
            destinationUri = null,
            bytes = null,
            code = null,
            warningCodes = emptyList(),
            updatedAtMillis = System.currentTimeMillis(),
            sourceKind = sourceKind,
          )
          withContext(Dispatchers.IO) { saveSafDestinationState(state) }
          ownsPersistedState = true
          if (cancellation.get()) {
            throw SafDestinationException(
              "cancelled",
              emptyList(),
              "Android SAF destination copy was cancelled",
            )
          }
          pendingSafDestination = PendingSafDestination(
            requestId,
            exportId,
            sourceKind,
            source,
            cancellation,
          )
          safDestinationPicker.launch(safeSafDestinationName(suggestedName))
        } catch (error: SafDestinationException) {
          terminalRecord = destinationTerminalRecord(
            requestId = requestId,
            exportId = managedExportIdFromPath(sourcePath) ?: requestId,
            phase = if (error.code == "cancelled") {
              SafDestinationPhase.CANCELLED
            } else {
              SafDestinationPhase.FAILED
            },
            code = error.code,
            warningCodes = error.warningCodes,
            sourceKind = sourceKind,
          )
          terminalMessage = error.message
        } catch (error: Exception) {
          terminalRecord = destinationTerminalRecord(
            requestId = requestId,
            exportId = managedExportIdFromPath(sourcePath) ?: requestId,
            phase = SafDestinationPhase.FAILED,
            code = "destination-state-failed",
            warningCodes = emptyList(),
            sourceKind = sourceKind,
          )
          terminalMessage = "Android SAF destination state could not be persisted"
        }
        terminalRecord?.let { record ->
          if (ownsPersistedState) persistTerminalIfPossible(record)
          safDestinationCancellations.remove(requestId, cancellation)
          safPickerSlot.release()
          dispatchSafDestination(record, terminalMessage)
        }
      }
    }

    fun cancelExport(requestId: String): Boolean {
      val cancellation = safDestinationCancellations[requestId] ?: return false
      cancellation.set(true)
      return synchronized(safDestinationStateLock) {
        val record = runCatching { safDestinationStateStore.load() }.getOrNull()
          ?.takeIf { it.requestId == requestId && !it.isTerminal() }
          ?: return@synchronized false
        runCatching {
          safDestinationStateStore.save(
            record.copy(
              phase = SafDestinationPhase.CANCELLING,
              updatedAtMillis = System.currentTimeMillis(),
            ),
          )
        }.isSuccess
      }
    }

    fun cancelSource(requestId: String) {
      safSourceCancellations[requestId]?.set(true)
    }

    fun discardSource(token: String): Boolean = safSpoolStore().discardReady(token)

    fun getActiveSourceRequestIds(): String = safSourceCancellations.keys
      .filter(::isCanonicalUuidV4)
      .sorted()
      .take(16)
      .joinToString(prefix = "[", separator = ",", postfix = "]") { "\"$it\"" }

    fun getExportStatus(): String? {
      val record = loadSafDestinationState()
        ?.takeIf(SafDestinationRecord::isTerminal)
        ?: return null
      return destinationJson(record, destinationMessage(record))
    }

    fun getExportSourceId(): String? = loadSafDestinationState()?.exportId

    fun markExportPublicationReady(requestId: String): Boolean {
      if (!isCanonicalUuidV4(requestId)) return false
      return synchronized(safDestinationStateLock) {
        runCatching {
          val record = safDestinationStateStore.load() ?: return@runCatching false
          val completed = completedSafPublicationPrerequisites(
            record,
            requestId,
            System.currentTimeMillis(),
          ) ?: return@runCatching false
          safDestinationStateStore.save(completed)
          true
        }.getOrDefault(false)
      }
    }

    fun acknowledgeExport(requestId: String): Boolean = acknowledgeSafDestinationExport(
      requestId,
      ::loadSafDestinationState,
      { record ->
        requiresRisuSavePublicationProof(dataDir, record.exportId, record.sourceKind)
      },
      { record ->
        prepareManagedExportAcknowledgement(dataDir, record.exportId, record.sourceKind)
      },
      ::clearSafDestinationState,
    )
  }

  private fun onBackupSourceSelected(uri: Uri?) {
    val pending = pendingBackupSource ?: return
    pendingBackupSource = null
    if (uri == null) {
      finishBackupSourcePick(pending, SafSpoolBatch(emptyList(), emptyList()))
      return
    }
    safScope.launch {
      val copyContext = currentCoroutineContext()
      val batch = try {
        val store = safSpoolStore()
        val source = withContext(Dispatchers.IO) {
          store.cleanupStale()
          val (displayName, totalBytes) = resolveSourceMetadata(uri)
          contentResolverSource(uri, displayName, totalBytes)
        }
        if (!isBackupSource(source.displayName)) {
          SafSpoolBatch(
            emptyList(),
            listOf(SafSpoolFailure(source.displayName, "unsupported-source")),
          )
        } else {
          spoolOpenedFilesOnIo(
            store,
            listOf(source),
            isCancelled = { pending.cancellation.get() || !copyContext.isActive },
            onProgress = { progress ->
              dispatchSafProgress(
                requestId = pending.requestId,
                operation = "source-copy",
                copiedBytes = progress.copiedBytes,
                totalBytes = progress.totalBytes,
                token = progress.token,
                dispatchKey = "${pending.requestId}:${progress.token}",
                isActive = { safSourceCancellations[pending.requestId] === pending.cancellation },
              )
            },
          )
        }
      } catch (error: Exception) {
        SafSpoolBatch(
          emptyList(),
          listOf(SafSpoolFailure("backup.risunest", "source-copy-failed")),
        )
      }
      if (!copyContext.isActive) return@launch
      val terminalBatch = if (pending.cancellation.get()) {
        val remaining = withContext(Dispatchers.IO) {
          val store = safSpoolStore()
          batch.ready.filterNot { store.discardReady(it.token) }
        }
        SafSpoolBatch(
          remaining,
          remaining.map { SafSpoolFailure(it.displayName, "cleanup-failed") },
        )
      } else {
        batch
      }
      finishBackupSourcePick(pending, terminalBatch)
    }
  }

  private fun finishBackupSourcePick(
    pending: PendingBackupSource,
    batch: SafSpoolBatch,
  ) {
    safSourceCancellations.remove(pending.requestId, pending.cancellation)
    safProgressThrottle.clear(pending.requestId)
    safPickerSlot.release()
    lifecycleWebView?.evaluateJavascript(
      androidBackupSourceResultScript(pending.requestId, batch, pending.restored),
      null,
    )
  }

  private fun dispatchBackupSourceBatch(requestId: String, batch: SafSpoolBatch) {
    lifecycleWebView?.evaluateJavascript(androidBackupSourcePickedScript(requestId, batch), null)
  }

  private fun onLegacyBackupSourceSelected(uri: Uri?) {
    val pending = pendingLegacyBackupSource ?: return
    pendingLegacyBackupSource = null
    if (uri == null) {
      finishLegacyBackupSourcePick(pending, SafSpoolBatch(emptyList(), emptyList()))
      return
    }
    safScope.launch {
      val copyContext = currentCoroutineContext()
      val batch = try {
        val store = safSpoolStore()
        val source = withContext(Dispatchers.IO) {
          store.cleanupStale()
          val (displayName, totalBytes) = runCatching { resolveSourceMetadata(uri) }
            .getOrElse {
              safeSafDisplayName(uri.lastPathSegment ?: "opened-file") to null
            }
          contentResolverSource(uri, displayName, totalBytes)
        }
        if (!(if (pending.content) isNativeContentSource(source.displayName) else source.displayName.endsWith(".bin", ignoreCase = true))) {
          SafSpoolBatch(
            emptyList(),
            listOf(SafSpoolFailure(source.displayName, "unsupported-source")),
          )
        } else {
          spoolOpenedFilesOnIo(
            store,
            listOf(source),
            importDestination = pending.importDestination,
            isCancelled = { pending.cancellation.get() || !copyContext.isActive },
            onProgress = { progress ->
              dispatchSafProgress(
                requestId = pending.requestId,
                operation = "source-copy",
                copiedBytes = progress.copiedBytes,
                totalBytes = progress.totalBytes,
                token = progress.token,
                dispatchKey = "${pending.requestId}:${progress.token}",
                isActive = {
                  safSourceCancellations[pending.requestId] === pending.cancellation
                },
              )
            },
          )
        }
      } catch (error: Exception) {
        SafSpoolBatch(
          emptyList(),
          listOf(SafSpoolFailure("backup.bin", "source-copy-failed")),
        )
      }
      if (!copyContext.isActive) return@launch
      val terminalBatch = if (pending.cancellation.get()) {
        val remaining = withContext(Dispatchers.IO) {
          val store = safSpoolStore()
          batch.ready.filterNot { store.discardReady(it.token) }
        }
        SafSpoolBatch(
          remaining,
          remaining.map { SafSpoolFailure(it.displayName, "cleanup-failed") },
        )
      } else {
        batch
      }
      finishLegacyBackupSourcePick(pending, terminalBatch)
    }
  }

  private fun finishLegacyBackupSourcePick(
    pending: PendingLegacyBackupSource,
    batch: SafSpoolBatch,
  ) {
    safSourceCancellations.remove(pending.requestId, pending.cancellation)
    safProgressThrottle.clear(pending.requestId)
    safPickerSlot.release()
    lifecycleWebView?.evaluateJavascript(
      if (pending.content && pending.restored) androidSpoolBatchScript(pending.requestId,
        SafSpoolBatch(emptyList(), listOf(SafSpoolFailure("content", "source-reselect-required"))))
      else if (pending.content) androidContentSourcePickedScript(pending.requestId, batch)
      else androidLegacyBackupSourceResultScript(pending.requestId, batch, pending.restored),
      null,
    )
  }

  private fun dispatchDocumentSourceBatch(content: Boolean, requestId: String, batch: SafSpoolBatch) {
    lifecycleWebView?.evaluateJavascript(
      if (content) androidContentSourcePickedScript(requestId, batch) else androidLegacyBackupSourcePickedScript(requestId, batch),
      null,
    )
  }

  private fun onSafDestinationSelected(uri: Uri?) {
    val pending = pendingSafDestination ?: restorePendingSafDestination() ?: run {
      cleanupUnclaimedSafDestination(uri)
      return
    }
    pendingSafDestination = null
    safScope.launch {
      val copyContext = currentCoroutineContext()
      var terminalMessage: String? = null
      val selectedState = try {
        withContext(Dispatchers.IO) {
          claimSafDestinationSelection(pending, uri)
        }
      } catch (error: Exception) {
        val warnings = cleanupUnclaimedSafDestinationOnIo(uri)
        val failure = destinationTerminalRecord(
          requestId = pending.requestId,
          exportId = pending.exportId,
          phase = SafDestinationPhase.FAILED,
          code = "destination-state-failed",
          warningCodes = warnings,
          sourceKind = pending.sourceKind,
        )
        val persisted = withContext(Dispatchers.IO) {
          persistPendingSafDestinationFailure(failure)
        }
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressThrottle.clear(pending.requestId)
        if (persisted) safPickerSlot.release()
        dispatchSafDestination(failure, "Android SAF destination state could not be persisted")
        return@launch
      }
      if (selectedState == null) {
        val warnings = cleanupUnclaimedSafDestinationOnIo(uri)
        val terminal = withContext(Dispatchers.IO) {
          addSafDestinationWarnings(pending.requestId, warnings)
        }
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressThrottle.clear(pending.requestId)
        terminal?.let { dispatchSafDestination(it, destinationMessage(it)) }
        return@launch
      }
      if (selectedState.isTerminal()) {
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressThrottle.clear(pending.requestId)
        safPickerSlot.release()
        dispatchSafDestination(selectedState, destinationMessage(selectedState))
        return@launch
      }
      val destinationUri = Uri.parse(requireNotNull(selectedState.destinationUri))
      val terminalRecord = try {
        val source = pending.source ?: throw SafDestinationException(
          "invalid-source",
          interruptedSafDestinationWarnings {
            contentResolver.delete(destinationUri, null, null) > 0
          },
          "Android SAF export source did not survive process recreation",
        )
        val result = copySafDestinationOnIo(
          source = source,
          openDestination = {
            contentResolver.openOutputStream(destinationUri, "wt")
              ?: throw IOException("Android SAF provider did not open the destination")
          },
          deletePartial = { contentResolver.delete(destinationUri, null, null) > 0 },
          createdDocument = true,
          isCancelled = { pending.cancellation.get() || !copyContext.isActive },
          onProgress = { copiedBytes ->
            dispatchSafProgress(
              requestId = pending.requestId,
              operation = "destination-copy",
              copiedBytes = copiedBytes,
              totalBytes = source.length(),
              token = null,
              dispatchKey = pending.requestId,
              isActive = {
                safDestinationCancellations[pending.requestId] === pending.cancellation
              },
            )
          },
        )
        destinationTerminalRecord(
          requestId = pending.requestId,
          exportId = pending.exportId,
          phase = SafDestinationPhase.SUCCEEDED,
          bytes = result.bytes,
          warningCodes = result.warningCodes,
          sourceKind = pending.sourceKind,
        )
      } catch (error: SafDestinationException) {
        terminalMessage = error.message
        destinationTerminalRecord(
          requestId = pending.requestId,
          exportId = pending.exportId,
          phase = if (error.code == "cancelled") {
            SafDestinationPhase.CANCELLED
          } else {
            SafDestinationPhase.FAILED
          },
          code = error.code,
          warningCodes = error.warningCodes,
          sourceKind = pending.sourceKind,
        )
      } catch (error: Exception) {
        terminalMessage = "Android SAF destination copy failed"
        val warnings = if (destinationUri.scheme == ContentResolver.SCHEME_CONTENT) {
          withContext(Dispatchers.IO) {
            interruptedSafDestinationWarnings {
              contentResolver.delete(destinationUri, null, null) > 0
            }
          }
        } else {
          emptyList()
        }
        destinationTerminalRecord(
          requestId = pending.requestId,
          exportId = pending.exportId,
          phase = SafDestinationPhase.FAILED,
          code = "destination-write-failed",
          warningCodes = warnings,
          sourceKind = pending.sourceKind,
        )
      } finally {
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressThrottle.clear(pending.requestId)
      }
      val (publishedRecord, publishedMessage) = finalizeSafDestination(
        terminalRecord,
        terminalMessage,
        destinationUri,
      )
      safPickerSlot.release()
      dispatchSafDestination(publishedRecord, publishedMessage)
    }
  }

  private fun cleanupUnclaimedSafDestination(uri: Uri?) {
    if (uri == null || uri.scheme != ContentResolver.SCHEME_CONTENT) return
    val terminalRequestId = loadSafDestinationState()
      ?.takeIf(SafDestinationRecord::isTerminal)
      ?.requestId
    safScope.launch {
      val warnings = cleanupUnclaimedSafDestinationOnIo(uri)
      val terminal = terminalRequestId?.let { requestId ->
        withContext(Dispatchers.IO) { addSafDestinationWarnings(requestId, warnings) }
      }
      terminal?.let { dispatchSafDestination(it, destinationMessage(it)) }
    }
  }

  private suspend fun cleanupUnclaimedSafDestinationOnIo(uri: Uri?): List<String> =
    withContext(Dispatchers.IO) {
      if (uri == null || uri.scheme != ContentResolver.SCHEME_CONTENT) return@withContext emptyList()
      interruptedSafDestinationWarnings {
        contentResolver.delete(uri, null, null) > 0
      }
    }

  private suspend fun finalizeSafDestination(
    record: SafDestinationRecord,
    message: String?,
    destinationUri: Uri?,
  ): Pair<SafDestinationRecord, String?> {
    if (persistTerminalIfPossible(record)) return record to message
    if (record.phase != SafDestinationPhase.SUCCEEDED) return record to message
    if (clearSafDestinationState(record.requestId)) {
      return record.copy(
        warningCodes = (
          record.warningCodes + "destination-state-not-persisted"
          ).distinct(),
      ) to message
    }
    val cleanupWarnings = withContext(Dispatchers.IO) {
      interruptedSafDestinationWarnings {
        destinationUri != null &&
          destinationUri.scheme == ContentResolver.SCHEME_CONTENT &&
          contentResolver.delete(destinationUri, null, null) > 0
      }
    }
    val failed = destinationTerminalRecord(
      requestId = record.requestId,
      exportId = record.exportId,
      phase = SafDestinationPhase.FAILED,
      code = "destination-state-failed",
      warningCodes = cleanupWarnings,
      sourceKind = record.sourceKind,
    )
    persistTerminalIfPossible(failed)
    return failed to "Android SAF destination result could not be persisted"
  }

  private fun managedExportIdFromPath(sourcePath: String): String? =
    runCatching { managedExportId(File(sourcePath)) }.getOrNull()

  private fun destinationTerminalRecord(
    requestId: String,
    exportId: String,
    phase: SafDestinationPhase,
    bytes: Long? = null,
    code: String? = null,
    warningCodes: List<String>,
    sourceKind: SafDestinationSourceKind = SafDestinationSourceKind.RISU_SAVE,
  ) = SafDestinationRecord(
    requestId = requestId,
    exportId = exportId,
    phase = phase,
    destinationUri = null,
    bytes = bytes,
    code = code,
    warningCodes = warningCodes,
    updatedAtMillis = System.currentTimeMillis(),
    sourceKind = sourceKind,
  )

  private suspend fun persistTerminalIfPossible(record: SafDestinationRecord): Boolean =
    withContext(Dispatchers.IO) {
      runCatching { saveSafDestinationState(record) }.isSuccess
    }

  private fun loadSafDestinationState(): SafDestinationRecord? = synchronized(
    safDestinationStateLock,
  ) {
    runCatching { safDestinationStateStore.load() }.getOrNull()
  }

  private fun saveSafDestinationState(record: SafDestinationRecord) = synchronized(
    safDestinationStateLock,
  ) {
    safDestinationStateStore.save(record)
  }

  private fun claimSafDestinationSelection(
    pending: PendingSafDestination,
    uri: Uri?,
  ): SafDestinationRecord? = synchronized(safDestinationStateLock) {
    val current = safDestinationStateStore.load() ?: return@synchronized null
    val selected = if (uri != null && uri.scheme != ContentResolver.SCHEME_CONTENT) {
      if (!isPendingSafDestinationPicker(current) || current.requestId != pending.requestId) {
        return@synchronized null
      }
      current.copy(
        phase = SafDestinationPhase.FAILED,
        destinationUri = null,
        bytes = null,
        code = "invalid-destination",
        warningCodes = emptyList(),
        updatedAtMillis = System.currentTimeMillis(),
      )
    } else {
      selectedSafDestinationState(
        current,
        pending.requestId,
        uri?.toString(),
        pending.cancellation.get(),
        System.currentTimeMillis(),
      ) ?: return@synchronized null
    }
    safDestinationStateStore.save(selected)
    selected
  }

  private fun persistPendingSafDestinationFailure(failure: SafDestinationRecord): Boolean =
    synchronized(safDestinationStateLock) {
      val ownsState = runCatching { safDestinationStateStore.load() }.getOrNull()
        ?.let { it.requestId == failure.requestId && !it.isTerminal() }
        ?: false
      if (!ownsState) return@synchronized false
      runCatching { safDestinationStateStore.save(failure) }.isSuccess
    }

  private fun addSafDestinationWarnings(
    requestId: String,
    warnings: List<String>,
  ): SafDestinationRecord? = synchronized(safDestinationStateLock) {
    val current = runCatching { safDestinationStateStore.load() }.getOrNull()
      ?.takeIf { it.requestId == requestId && it.isTerminal() }
      ?: return@synchronized null
    val merged = (current.warningCodes + warnings).distinct()
    if (merged == current.warningCodes) return@synchronized current
    val updated = current.copy(
      warningCodes = merged,
      updatedAtMillis = System.currentTimeMillis(),
    )
    return@synchronized runCatching {
      safDestinationStateStore.save(updated)
      updated
    }.getOrNull()
  }

  private fun expireSafDestinationPicker(
    requestId: String,
    nowMillis: Long,
  ): SafDestinationRecord? = synchronized(safDestinationStateLock) {
    val current = safDestinationStateStore.load() ?: return@synchronized null
    val expired = expiredSafDestinationState(current, requestId, nowMillis)
      ?: return@synchronized null
    safDestinationStateStore.save(expired)
    expired
  }

  private fun clearSafDestinationState(requestId: String): Boolean = synchronized(
    safDestinationStateLock,
  ) {
    runCatching { safDestinationStateStore.clear(requestId) }.getOrDefault(false)
  }

  private fun restorePendingSafDestination(): PendingSafDestination? {
    val record = loadSafDestinationState()
      ?.takeIf(::isPendingSafDestinationPicker)
      ?: return null
    val cancellation = safDestinationCancellations.computeIfAbsent(record.requestId) {
      AtomicBoolean(record.phase == SafDestinationPhase.CANCELLING)
    }
    return PendingSafDestination(
      requestId = record.requestId,
      exportId = record.exportId,
      sourceKind = record.sourceKind,
      source = resolveManagedExportById(dataDir, record.exportId),
      cancellation = cancellation,
    )
  }

  private fun recoverSafDestination(hasRestoredActivityState: Boolean) {
    val record = loadSafDestinationState() ?: return
    when (decideSafDestinationRecovery(record, hasRestoredActivityState)) {
      SafDestinationRecoveryAction.WAIT_FOR_PICKER -> {
        safPickerSlot.acquireRestored()
        pendingSafDestination = restorePendingSafDestination()
        schedulePendingDestinationExpiry(record)
      }
      SafDestinationRecoveryAction.CLEAN_PARTIAL -> {
        safPickerSlot.acquireRestored()
        safScope.launch {
          try {
            val destinationUri = record.destinationUri?.let(Uri::parse)
            val warnings = withContext(Dispatchers.IO) {
              interruptedSafDestinationWarnings {
                destinationUri != null &&
                  destinationUri.scheme == ContentResolver.SCHEME_CONTENT &&
                  contentResolver.delete(destinationUri, null, null) > 0
              }
            }
            val wasCancelling = record.phase == SafDestinationPhase.CANCELLING
            val terminal = destinationTerminalRecord(
              requestId = record.requestId,
              exportId = record.exportId,
              phase = if (wasCancelling) {
                SafDestinationPhase.CANCELLED
              } else {
                SafDestinationPhase.FAILED
              },
              code = if (wasCancelling) "cancelled" else "destination-interrupted",
              warningCodes = warnings,
              sourceKind = record.sourceKind,
            )
            persistTerminalIfPossible(terminal)
            dispatchSafDestination(terminal, destinationMessage(terminal))
          } finally {
            safPickerSlot.release()
          }
        }
      }
      SafDestinationRecoveryAction.FAIL_INTERRUPTED -> {
        safPickerSlot.acquireRestored()
        safScope.launch {
          try {
            val wasCancelling = record.phase == SafDestinationPhase.CANCELLING
            val terminal = destinationTerminalRecord(
              requestId = record.requestId,
              exportId = record.exportId,
              phase = if (wasCancelling) {
                SafDestinationPhase.CANCELLED
              } else {
                SafDestinationPhase.FAILED
              },
              code = if (wasCancelling) "cancelled" else "destination-interrupted",
              warningCodes = emptyList(),
              sourceKind = record.sourceKind,
            )
            persistTerminalIfPossible(terminal)
            dispatchSafDestination(terminal, destinationMessage(terminal))
          } finally {
            safPickerSlot.release()
          }
        }
      }
      SafDestinationRecoveryAction.REPLAY_TERMINAL -> Unit
    }
  }

  private fun schedulePendingDestinationExpiry(record: SafDestinationRecord) {
    val delayMillis = (
      record.updatedAtMillis + DESTINATION_PICKER_STALE_MILLIS - System.currentTimeMillis()
      ).coerceAtLeast(1L)
    mainHandler.postDelayed({
      safScope.launch {
        val terminal = withContext(Dispatchers.IO) {
          runCatching {
            expireSafDestinationPicker(record.requestId, System.currentTimeMillis())
          }.getOrNull()
        }
          ?: return@launch
        safDestinationCancellations[terminal.requestId]?.set(true)
        if (pendingSafDestination?.requestId == terminal.requestId) {
          pendingSafDestination = null
        }
        safDestinationCancellations.remove(terminal.requestId)
        safPickerSlot.release()
        dispatchSafDestination(terminal, destinationMessage(terminal))
      }
    }, delayMillis)
  }

  private fun replaySafDestinationResult(webView: WebView) {
    safScope.launch {
      val record = withContext(Dispatchers.IO) {
        loadSafDestinationState()?.takeIf(SafDestinationRecord::isTerminal)
      } ?: return@launch
      if (lifecycleWebView !== webView || !frontendReady.isReady) return@launch
      webView.evaluateJavascript(
        androidSafDestinationScriptForRecord(record, destinationMessage(record)),
        null,
      )
    }
  }

  private fun dispatchSafDestination(record: SafDestinationRecord, message: String?) {
    if (!frontendReady.isReady) return
    lifecycleWebView?.evaluateJavascript(
      androidSafDestinationScriptForRecord(record, message),
      null,
    )
  }

  private fun destinationJson(record: SafDestinationRecord, message: String?) =
    androidSafDestinationJson(
      requestId = record.requestId,
      exportId = record.exportId,
      sourceKind = record.sourceKind,
      state = record.phase.wireName,
      bytes = record.bytes,
      code = record.code,
      message = message,
      warningCodes = record.warningCodes,
      publicationPrerequisitesComplete = record.publicationPrerequisitesComplete,
    )

  private fun destinationMessage(record: SafDestinationRecord): String? = when (record.code) {
    "cancelled" -> "Android SAF destination copy was cancelled"
    "destination-interrupted" -> "Android SAF destination copy was interrupted"
    "destination-state-failed" -> "Android SAF destination state could not be persisted"
    "invalid-source" -> "Android SAF export source is unavailable"
    "invalid-destination" -> "Android SAF destination is invalid"
    "destination-write-failed" -> "Android SAF destination copy failed"
    else -> null
  }

  private fun injectOpenedFiles(
    webView: WebView,
    openedIntent: Intent? = intent,
  ) {
    val uris = claimOpenedFileUris(openedIntent)
    if (uris.isEmpty()) return
    val ready = frontendReady
    val requestId = UUID.randomUUID().toString()
    val cancellation = AtomicBoolean(false)
    safSourceCancellations[requestId] = cancellation
    safScope.launch {
      try {
        val openedSources = withContext(Dispatchers.IO) {
          uris.map { uri ->
            val (displayName, totalBytes) = runCatching { resolveSourceMetadata(uri) }
              .getOrElse { safeSafDisplayName(uri.lastPathSegment ?: "opened-file") to null }
            OpenedFileSource(uri, displayName, totalBytes)
          }
        }
        val (nativeJobSources, legacySources) = openedSources.partition { source ->
          shouldUseNativeFileJobSpool(source.displayName)
        }
        stageLegacyOpenedFiles(webView, legacySources.map { it.uri }, ready)
        if (nativeJobSources.isEmpty()) return@launch
        val store = safSpoolStore()
        val sources = withContext(Dispatchers.IO) {
          store.cleanupStale()
          nativeJobSources.map { source ->
            contentResolverSource(source.uri, source.displayName, source.totalBytes)
          }
        }
        val copyContext = currentCoroutineContext()
        val batch = spoolOpenedFilesOnIo(
          store,
          sources,
          isCancelled = { cancellation.get() || !copyContext.isActive },
          onProgress = { progress ->
            dispatchSafProgress(
              requestId = requestId,
              operation = "source-copy",
              copiedBytes = progress.copiedBytes,
              totalBytes = progress.totalBytes,
              token = progress.token,
              dispatchKey = "$requestId:${progress.token}",
              isActive = { safSourceCancellations[requestId] === cancellation },
            )
          },
        )
        ready.await()
        if (!copyContext.isActive || lifecycleWebView !== webView) return@launch
        val newlyReady = batch.ready.filter { deliveredSpoolTokens.add(it.token) }
        webView.evaluateJavascript(
          androidSpoolBatchScript(requestId, batch.copy(ready = newlyReady)),
          null,
        )
      } finally {
        safSourceCancellations.remove(requestId, cancellation)
        safProgressThrottle.clear(requestId)
      }
    }
  }

  private fun replayReadySpools(webView: WebView) {
    safScope.launch {
      val ready = withContext(Dispatchers.IO) {
        val store = safSpoolStore()
        store.cleanupStale()
        store.listReady().filter { shouldUseNativeFileJobSpool(it.displayName) }
      }
      if (ready.isEmpty() || lifecycleWebView !== webView || !frontendReady.isReady) return@launch
      val newlyReady = ready.filter { deliveredSpoolTokens.add(it.token) }
      if (newlyReady.isEmpty()) return@launch
      webView.evaluateJavascript(
        androidSpoolBatchScript(UUID.randomUUID().toString(), SafSpoolBatch(newlyReady, emptyList())),
        null,
      )
    }
  }

  private fun safSpoolStore() = SafSpoolStore(
    root = File(dataDir, "native-file-jobs/sources"),
    atomicPublisher = AndroidSafAtomicPublisher,
  )

  private fun claimOpenedFileUris(openedIntent: Intent?): List<Uri> {
    openedIntent ?: return emptyList()
    val uris = launchOpenedFileUris(openedIntent)
    if (uris.isEmpty()) return emptyList()
    val fingerprint = openedFileIntentFingerprint(
      openedIntent.action,
      uris.map(Uri::toString),
    )
    val marker = RestoredIntentConsumptionMarker(
      isConsumed = {
        openedIntent.getBooleanExtra(OPENED_FILE_INTENT_CONSUMED, false) ||
          consumedOpenedFileFingerprint == fingerprint
      },
      markConsumed = {
        openedIntent.putExtra(OPENED_FILE_INTENT_CONSUMED, true)
        consumedOpenedFileFingerprint = fingerprint
      },
    )
    return if (marker.claim()) uris else emptyList()
  }

  private fun dispatchSafProgress(
    requestId: String,
    operation: String,
    copiedBytes: Long,
    totalBytes: Long?,
    token: String?,
    dispatchKey: String,
    isActive: () -> Boolean,
  ) {
    val target = lifecycleWebView ?: return
    val now = SystemClock.elapsedRealtime()
    if (!safProgressThrottle.shouldDispatch(dispatchKey, now)) return
    val script = androidSafProgressScript(
      requestId,
      operation,
      copiedBytes,
      totalBytes,
      token,
    )
    mainHandler.post {
      if (lifecycleWebView === target && isActive()) {
        target.evaluateJavascript(script, null)
      }
    }
  }

  private fun stageLegacyOpenedFiles(
    webView: WebView,
    uris: List<Uri>,
    ready: AndroidFrontendReady = frontendReady,
  ) {
    if (uris.isEmpty()) return
    safScope.launch {
      val openedFiles = withContext(Dispatchers.IO) { copyLegacyOpenedFiles(uris) }
      if (openedFiles.isEmpty()) return@launch
      ready.await()
      if (lifecycleWebView === webView) {
        webView.evaluateJavascript(openedFilesEventScript(openedFiles), null)
      }
    }
  }

  private fun copyLegacyOpenedFiles(uris: List<Uri>): List<String> {
    if (uris.isEmpty()) return emptyList()
    val directory = File(cacheDir, "opened_files")
    directory.mkdirs()
    val stamp = System.currentTimeMillis()
    cleanupLegacyOpenedFiles(directory, stamp)
    return uris.mapIndexedNotNull { index, uri ->
      var target: File? = null
      try {
        val openedTarget = File(directory, "${UUID.randomUUID()}-$index-${resolveLegacyDisplayName(uri)}")
        target = openedTarget
        contentResolver.openInputStream(uri)?.use { input ->
          openedTarget.outputStream().use { output -> input.copyTo(output) }
        } ?: return@mapIndexedNotNull null
        openedTarget.absolutePath
      } catch (error: Exception) {
        target?.delete()
        null
      }
    }
  }

  private fun resolveLegacyDisplayName(uri: Uri): String {
    if (uri.scheme == ContentResolver.SCHEME_CONTENT) {
      contentResolver.query(
        uri,
        arrayOf(OpenableColumns.DISPLAY_NAME),
        null,
        null,
        null,
      )?.use { cursor ->
        val column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
        if (column >= 0 && cursor.moveToFirst()) {
          val name = cursor.getString(column)
          if (!name.isNullOrBlank()) return sanitizeOpenedFileName(name)
        }
      }
    }
    return sanitizeOpenedFileName(uri.lastPathSegment ?: "opened-file")
  }

  private fun showRendererRecoveryWarning() {
    if (rendererRecoveryMarker.consume()) {
      Toast.makeText(this, R.string.renderer_recovery_warning, Toast.LENGTH_LONG).show()
    }
  }

  private fun diagnoseWebViewProvider() {
    val provider = WebViewCompat.getCurrentWebViewPackage(this)
    val decision = decideWebViewProvider(provider?.packageName, provider?.versionName)
    val hasDocumentStartScript = WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)
    Log.i(
      TAG,
      "Android WebView provider=${provider?.packageName ?: "missing"}, " +
        "version=${provider?.versionName ?: "missing"}, " +
        "major=${decision.majorVersion ?: "unknown"}, API=${Build.VERSION.SDK_INT}, " +
        "documentStartScript=$hasDocumentStartScript",
    )

    val warning = when (decision.status) {
      WebViewProviderStatus.SUPPORTED -> null
      WebViewProviderStatus.MISSING -> getString(R.string.webview_provider_missing)
      WebViewProviderStatus.OUTDATED -> getString(
        R.string.webview_provider_outdated,
        provider?.versionName ?: "unknown",
        MINIMUM_WEBVIEW_MAJOR,
      )
      WebViewProviderStatus.UNKNOWN_VERSION -> getString(
        R.string.webview_provider_unknown_version,
        provider?.packageName ?: "unknown",
      )
    }
    if (warning != null) {
      Toast.makeText(this, warning, Toast.LENGTH_LONG).show()
    }
  }

  private fun launchOpenedFileUris(intent: Intent?): List<Uri> {
    intent ?: return emptyList()
    return when (intent.action) {
      Intent.ACTION_VIEW, "org.chromium.arc.intent.action.VIEW" -> listOfNotNull(intent.data)
      Intent.ACTION_SEND ->
        listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
      Intent.ACTION_SEND_MULTIPLE ->
        IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
          ?.filterNotNull()
          .orEmpty()
      else -> emptyList()
    }
  }

  private fun contentResolverSource(
    uri: Uri,
    displayName: String,
    totalBytes: Long?,
  ): SafInputSource {
    return object : SafInputSource {
      override val displayName = displayName
      override val totalBytes = totalBytes

      override fun open(): InputStream = contentResolver.openInputStream(uri)
        ?: throw IOException("Android SAF provider did not open the source")
    }
  }

  private fun resolveSourceMetadata(uri: Uri): Pair<String, Long?> {
    if (uri.scheme == ContentResolver.SCHEME_CONTENT) {
      contentResolver.query(
        uri,
        arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE),
        null,
        null,
        null,
      )?.use { cursor ->
        if (cursor.moveToFirst()) {
          val nameColumn = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
          val sizeColumn = cursor.getColumnIndex(OpenableColumns.SIZE)
          val name = if (nameColumn >= 0) cursor.getString(nameColumn) else null
          val size = if (sizeColumn >= 0 && !cursor.isNull(sizeColumn)) {
            cursor.getLong(sizeColumn).takeIf { it >= 0 }
          } else {
            null
          }
          if (!name.isNullOrBlank()) return safeSafDisplayName(name) to size
        }
      }
    }
    return safeSafDisplayName(uri.lastPathSegment ?: "opened-file") to null
  }
}
