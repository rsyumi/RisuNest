package co.aiclient.risu

import android.content.ComponentCallbacks2
import android.content.ContentResolver
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.OpenableColumns
import android.util.Log
import android.view.ViewGroup
import android.webkit.JavascriptInterface
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
private const val LIFECYCLE_BRIDGE_NAME = "RisuLifecycleBridge"
private const val SAF_BRIDGE_NAME = "RisuSafBridge"
private const val STOP_REASON = "stop"
private const val TRIM_MEMORY_REASON = "trim-memory"
private const val EXIT_REASON = "exit"
// Vite 8's pinned Baseline target starts at Chrome 111. Update this with the web build target.
private const val MINIMUM_WEBVIEW_MAJOR = 111
private const val NATIVE_RESILIENCE_PREFERENCES = "risu-native-resilience"
private const val RENDERER_RECOVERY_MARKER = "renderer-recovery-warning"
private const val SAF_PROGRESS_INTERVAL_MILLIS = 100L
private const val OPENED_FILE_INTENT_CONSUMED = "co.aiclient.risu.OPENED_FILE_INTENT_CONSUMED"
private const val OPENED_FILE_FINGERPRINT_STATE = "risu.opened-file-fingerprint"
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
        logFailure(name, error)
      }
    }
    return try {
      restart()
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

internal fun openedFilesScript(paths: List<String>): String {
  val values = paths.joinToString(",") { "\"${escapeJsStringLiteral(it)}\"" }
  return "window.tauriOpenedFiles=[$values];"
}

internal fun shouldUseNativeRisuSaveSpool(displayName: String): Boolean =
  displayName.endsWith(".risudat", ignoreCase = true)

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
  val source: File?,
  val cancellation: AtomicBoolean,
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

class MainActivity : TauriActivity(), RendererRecoveryHost {
  private val backNavigationPolicy = BackNavigationPolicy()
  private var lifecycleWebView: WebView? = null
  private val lifecycleFlushDispatcher = LifecycleFlushDispatcher(::dispatchLifecycleFlush)
  private val exitFlushGate = ExitFlushGate()
  private val rendererRecoveryCoordinator = RendererRecoveryCoordinator(::logRendererRecoveryFailure)
  private val mainHandler = Handler(Looper.getMainLooper())
  private val safScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
  private val safSourceCancellations = ConcurrentHashMap<String, AtomicBoolean>()
  private val safDestinationCancellations = ConcurrentHashMap<String, AtomicBoolean>()
  private val safProgressDispatchMillis = ConcurrentHashMap<String, Long>()
  private val deliveredSpoolTokens = ConcurrentHashMap.newKeySet<String>()
  private var pendingSafDestination: PendingSafDestination? = null
  private val safDestinationSlot = SafDestinationSlot()
  private val safDestinationStateLock = Any()
  private var consumedOpenedFileFingerprint: String? = null
  private val safDestinationStateStore by lazy {
    SafDestinationStateStore(
      File(dataDir, "native-file-jobs/android-saf-destination.json"),
      AndroidSafAtomicPublisher,
    )
  }
  private val safDestinationPicker = registerForActivityResult(
    ActivityResultContracts.CreateDocument("application/octet-stream"),
    ::onSafDestinationSelected,
  )
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
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      recoverSafDestination(savedInstanceState != null)
    }
    showRendererRecoveryWarning()
    diagnoseWebViewProvider()
  }

  override fun recoverRenderer(webView: WebView, didCrash: Boolean): Boolean {
    Log.e(TAG, "Android WebView renderer exited, didCrash=$didCrash")
    return rendererRecoveryCoordinator.recover(
      removeFromParent = { (webView.parent as? ViewGroup)?.removeView(webView) },
      removeJavascriptBridge = {
        webView.removeJavascriptInterface(LIFECYCLE_BRIDGE_NAME)
        webView.removeJavascriptInterface(SAF_BRIDGE_NAME)
      },
      destroyView = webView::destroy,
      clearReference = {
        if (lifecycleWebView === webView) {
          lifecycleWebView = null
        }
      },
      markRecovery = rendererRecoveryMarker::mark,
      restart = coldRestartDispatcher::restart,
    )
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    lifecycleWebView = webView
    webView.addJavascriptInterface(LifecycleFlushBridge(), LIFECYCLE_BRIDGE_NAME)
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      deliveredSpoolTokens.clear()
      webView.addJavascriptInterface(SafBridge(), SAF_BRIDGE_NAME)
      replayReadySpools(webView)
      replaySafDestinationResult(webView)
      injectOpenedFiles(webView)
    } else {
      injectLegacyOpenedFiles(webView)
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

  override fun onSaveInstanceState(outState: Bundle) {
    consumedOpenedFileFingerprint?.let {
      outState.putString(OPENED_FILE_FINGERPRINT_STATE, it)
    }
    super.onSaveInstanceState(outState)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      setIntent(intent)
      consumedOpenedFileFingerprint = null
      lifecycleWebView?.let { injectOpenedFiles(it, intent, includeLegacyFiles = false) }
    }
  }

  override fun onDestroy() {
    safSourceCancellations.values.forEach { it.set(true) }
    safDestinationCancellations.values.forEach { it.set(true) }
    pendingSafDestination = null
    safScope.cancel()
    safSourceCancellations.clear()
    safDestinationCancellations.clear()
    safProgressDispatchMillis.clear()
    lifecycleWebView?.removeJavascriptInterface(SAF_BRIDGE_NAME)
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

  private inner class LifecycleFlushBridge {
    @JavascriptInterface
    fun onFlushComplete(token: String?) {
      token ?: return
      mainHandler.post { finishForExitFlush(token) }
    }

    @JavascriptInterface
    fun onFlushHold(token: String?) {
      token ?: return
      mainHandler.post { exitFlushGate.cancel(token) }
    }

    @JavascriptInterface
    fun requestExit() {
      mainHandler.post { finishAndRemoveTask() }
    }

    @JavascriptInterface
    fun requestRestart() {
      mainHandler.post { coldRestartDispatcher.restart() }
    }
  }

  private inner class SafBridge {
    @JavascriptInterface
    fun copyExport(
      requestId: String,
      sourcePath: String,
      suggestedName: String,
    ) {
      if (!isCanonicalUuidV4(requestId)) return
      if (!safDestinationSlot.tryAcquire()) {
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
          )
          terminalMessage = error.message
        } catch (error: Exception) {
          terminalRecord = destinationTerminalRecord(
            requestId = requestId,
            exportId = managedExportIdFromPath(sourcePath) ?: requestId,
            phase = SafDestinationPhase.FAILED,
            code = "destination-state-failed",
            warningCodes = emptyList(),
          )
          terminalMessage = "Android SAF destination state could not be persisted"
        }
        terminalRecord?.let { record ->
          if (ownsPersistedState) persistTerminalIfPossible(record)
          safDestinationCancellations.remove(requestId, cancellation)
          safDestinationSlot.release()
          dispatchSafDestination(record, terminalMessage)
        }
      }
    }

    @JavascriptInterface
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

    @JavascriptInterface
    fun cancelSource(requestId: String) {
      safSourceCancellations[requestId]?.set(true)
    }

    @JavascriptInterface
    fun discardSource(token: String): Boolean = safSpoolStore().discardReady(token)

    @JavascriptInterface
    fun getActiveSourceRequestIds(): String = safSourceCancellations.keys
      .filter(::isCanonicalUuidV4)
      .sorted()
      .take(16)
      .joinToString(prefix = "[", separator = ",", postfix = "]") { "\"$it\"" }

    @JavascriptInterface
    fun getExportStatus(): String? {
      val record = loadSafDestinationState()
        ?.takeIf(SafDestinationRecord::isTerminal)
        ?: return null
      return destinationJson(record, destinationMessage(record))
    }

    @JavascriptInterface
    fun acknowledgeExport(requestId: String): Boolean {
      if (!isCanonicalUuidV4(requestId)) return false
      return clearSafDestinationState(requestId)
    }
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
        )
        val persisted = withContext(Dispatchers.IO) {
          persistPendingSafDestinationFailure(failure)
        }
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressDispatchMillis.remove(pending.requestId)
        if (persisted) safDestinationSlot.release()
        dispatchSafDestination(failure, "Android SAF destination state could not be persisted")
        return@launch
      }
      if (selectedState == null) {
        val warnings = cleanupUnclaimedSafDestinationOnIo(uri)
        val terminal = withContext(Dispatchers.IO) {
          addSafDestinationWarnings(pending.requestId, warnings)
        }
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressDispatchMillis.remove(pending.requestId)
        terminal?.let { dispatchSafDestination(it, destinationMessage(it)) }
        return@launch
      }
      if (selectedState.isTerminal()) {
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressDispatchMillis.remove(pending.requestId)
        safDestinationSlot.release()
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
        )
      } finally {
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
        safProgressDispatchMillis.remove(pending.requestId)
      }
      val (publishedRecord, publishedMessage) = finalizeSafDestination(
        terminalRecord,
        terminalMessage,
        destinationUri,
      )
      safDestinationSlot.release()
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
  ) = SafDestinationRecord(
    requestId = requestId,
    exportId = exportId,
    phase = phase,
    destinationUri = null,
    bytes = bytes,
    code = code,
    warningCodes = warningCodes,
    updatedAtMillis = System.currentTimeMillis(),
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
      source = resolveManagedExportById(dataDir, record.exportId),
      cancellation = cancellation,
    )
  }

  private fun recoverSafDestination(hasRestoredActivityState: Boolean) {
    val record = loadSafDestinationState() ?: return
    when (decideSafDestinationRecovery(record, hasRestoredActivityState)) {
      SafDestinationRecoveryAction.WAIT_FOR_PICKER -> {
        safDestinationSlot.acquireRestored()
        pendingSafDestination = restorePendingSafDestination()
        schedulePendingDestinationExpiry(record)
      }
      SafDestinationRecoveryAction.CLEAN_PARTIAL -> {
        safDestinationSlot.acquireRestored()
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
            )
            persistTerminalIfPossible(terminal)
            dispatchSafDestination(terminal, destinationMessage(terminal))
          } finally {
            safDestinationSlot.release()
          }
        }
      }
      SafDestinationRecoveryAction.FAIL_INTERRUPTED -> {
        safDestinationSlot.acquireRestored()
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
            )
            persistTerminalIfPossible(terminal)
            dispatchSafDestination(terminal, destinationMessage(terminal))
          } finally {
            safDestinationSlot.release()
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
        safDestinationSlot.release()
        dispatchSafDestination(terminal, destinationMessage(terminal))
      }
    }, delayMillis)
  }

  private fun replaySafDestinationResult(webView: WebView) {
    val record = loadSafDestinationState()
      ?.takeIf(SafDestinationRecord::isTerminal)
      ?: return
    webView.evaluateJavascript(
      androidSafDestinationScriptForRecord(record, destinationMessage(record)),
      null,
    )
  }

  private fun dispatchSafDestination(record: SafDestinationRecord, message: String?) {
    lifecycleWebView?.evaluateJavascript(
      androidSafDestinationScriptForRecord(record, message),
      null,
    )
  }

  private fun androidSafDestinationScriptForRecord(
    record: SafDestinationRecord,
    message: String?,
  ) = androidSafDestinationScript(
    requestId = record.requestId,
    state = record.phase.wireName,
    bytes = record.bytes,
    code = record.code,
    message = message,
    warningCodes = record.warningCodes,
  )

  private fun destinationJson(record: SafDestinationRecord, message: String?) =
    androidSafDestinationJson(
      requestId = record.requestId,
      state = record.phase.wireName,
      bytes = record.bytes,
      code = record.code,
      message = message,
      warningCodes = record.warningCodes,
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
    includeLegacyFiles: Boolean = true,
  ) {
    val uris = claimOpenedFileUris(openedIntent)
    if (uris.isEmpty()) return
    val (risuSaveUris, legacyUris) = uris.partition { uri ->
      val displayName = runCatching { resolveLegacyDisplayName(uri) }
        .getOrElse { sanitizeOpenedFileName(uri.lastPathSegment ?: "opened-file") }
      shouldUseNativeRisuSaveSpool(displayName)
    }
    if (includeLegacyFiles) injectLegacyOpenedFiles(webView, legacyUris)
    if (risuSaveUris.isEmpty()) return
    val requestId = UUID.randomUUID().toString()
    val cancellation = AtomicBoolean(false)
    safSourceCancellations[requestId] = cancellation
    safScope.launch {
      try {
        val store = safSpoolStore()
        val sources = withContext(Dispatchers.IO) {
          store.cleanupStale()
          risuSaveUris.map(::contentResolverSource)
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
        if (!copyContext.isActive || lifecycleWebView !== webView) return@launch
        val newlyReady = batch.ready.filter { deliveredSpoolTokens.add(it.token) }
        webView.evaluateJavascript(
          androidSpoolBatchScript(requestId, batch.copy(ready = newlyReady)),
          null,
        )
      } finally {
        safSourceCancellations.remove(requestId, cancellation)
        safProgressDispatchMillis.keys.removeAll {
          it == requestId || it.startsWith("$requestId:")
        }
      }
    }
  }

  private fun replayReadySpools(webView: WebView) {
    val requestId = UUID.randomUUID().toString()
    safScope.launch {
      val ready = withContext(Dispatchers.IO) {
        val store = safSpoolStore()
        store.cleanupStale()
        store.listReady().filter {
          shouldUseNativeRisuSaveSpool(it.displayName) && deliveredSpoolTokens.add(it.token)
        }
      }
      if (ready.isEmpty() || lifecycleWebView !== webView) return@launch
      webView.evaluateJavascript(
        androidSpoolBatchScript(requestId, SafSpoolBatch(ready, emptyList())),
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
    val now = SystemClock.elapsedRealtime()
    val previous = safProgressDispatchMillis.put(dispatchKey, now)
    if (previous != null && now - previous < SAF_PROGRESS_INTERVAL_MILLIS) return
    val target = lifecycleWebView ?: return
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

  private fun injectLegacyOpenedFiles(
    webView: WebView,
    uris: List<Uri> = launchOpenedFileUris(intent),
  ) {
    val openedFiles = copyLegacyOpenedFiles(uris)
    if (openedFiles.isEmpty()) return
    val script = openedFilesScript(openedFiles)
    if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
      WebViewCompat.addDocumentStartJavaScript(webView, script, setOf("*"))
    } else {
      webView.evaluateJavascript(script, null)
    }
  }

  private fun copyLegacyOpenedFiles(uris: List<Uri>): List<String> {
    if (uris.isEmpty()) return emptyList()
    val directory = File(cacheDir, "opened_files")
    directory.mkdirs()
    val stamp = System.currentTimeMillis()
    return uris.mapIndexedNotNull { index, uri ->
      try {
        val target = File(directory, "$stamp-$index-${resolveLegacyDisplayName(uri)}")
        contentResolver.openInputStream(uri)?.use { input ->
          target.outputStream().use { output -> input.copyTo(output) }
        } ?: return@mapIndexedNotNull null
        target.absolutePath
      } catch (error: Exception) {
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

  private fun contentResolverSource(uri: Uri): SafInputSource {
    val (displayName, totalBytes) = try {
      resolveSourceMetadata(uri)
    } catch (error: Exception) {
      safeSafDisplayName(uri.lastPathSegment ?: "opened-file") to null
    }
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
