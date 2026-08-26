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

internal fun sanitizeOpenedFileName(name: String): String = safeSafDisplayName(name)

private data class PendingSafDestination(
  val requestId: String,
  val source: File,
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
  private val safDestinationCancellations = ConcurrentHashMap<String, AtomicBoolean>()
  private var pendingSafDestination: PendingSafDestination? = null
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
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
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
      webView.addJavascriptInterface(SafBridge(), SAF_BRIDGE_NAME)
      injectOpenedFiles(webView)
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

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    if (BuildConfig.ENABLE_EXPERIMENTAL_SAF_FILE_JOBS) {
      lifecycleWebView?.let { injectOpenedFiles(it, intent) }
    }
  }

  override fun onDestroy() {
    safDestinationCancellations.values.forEach { it.set(true) }
    pendingSafDestination = null
    safScope.cancel()
    safDestinationCancellations.clear()
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
      if (requestId.isBlank() || requestId.length > 128) return
      val cancellation = AtomicBoolean(false)
      if (safDestinationCancellations.putIfAbsent(requestId, cancellation) != null) return
      safScope.launch {
        val script = try {
          val source = withContext(Dispatchers.IO) {
            resolveManagedExportSource(dataDir, sourcePath)
          } ?: throw SafDestinationException(
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
          if (pendingSafDestination != null) throw SafDestinationException(
            "destination-busy",
            emptyList(),
            "Another Android SAF destination picker is already open",
          )
          pendingSafDestination = PendingSafDestination(requestId, source, cancellation)
          safDestinationPicker.launch(safeSafDestinationName(suggestedName))
          null
        } catch (error: SafDestinationException) {
          androidSafDestinationScript(
            requestId = requestId,
            state = if (error.code == "cancelled") "cancelled" else "failed",
            code = error.code,
            message = error.message,
            warningCodes = error.warningCodes,
          )
        } catch (error: Exception) {
          androidSafDestinationScript(
            requestId = requestId,
            state = "failed",
            code = "destination-write-failed",
            message = "Android SAF destination copy failed",
            warningCodes = emptyList(),
          )
        }
        if (script != null) {
          safDestinationCancellations.remove(requestId, cancellation)
          lifecycleWebView?.evaluateJavascript(script, null)
        }
      }
    }

    @JavascriptInterface
    fun cancelExport(requestId: String) {
      safDestinationCancellations[requestId]?.set(true)
    }
  }

  private fun onSafDestinationSelected(uri: Uri?) {
    val pending = pendingSafDestination ?: return
    pendingSafDestination = null
    if (uri == null) {
      safDestinationCancellations.remove(pending.requestId, pending.cancellation)
      lifecycleWebView?.evaluateJavascript(
        androidSafDestinationScript(
          requestId = pending.requestId,
          state = "cancelled",
          code = "cancelled",
          message = "Android SAF destination selection was cancelled",
          warningCodes = emptyList(),
        ),
        null,
      )
      return
    }
    safScope.launch {
      val copyContext = currentCoroutineContext()
      val script = try {
        if (uri.scheme != ContentResolver.SCHEME_CONTENT) {
          throw SafDestinationException(
            "invalid-destination",
            emptyList(),
            "Android SAF destination must be a content URI",
          )
        }
        val result = copySafDestinationOnIo(
          source = pending.source,
          openDestination = {
            contentResolver.openOutputStream(uri, "wt")
              ?: throw IOException("Android SAF provider did not open the destination")
          },
          deletePartial = { contentResolver.delete(uri, null, null) > 0 },
          createdDocument = true,
          isCancelled = { pending.cancellation.get() || !copyContext.isActive },
        )
        androidSafDestinationScript(
          requestId = pending.requestId,
          state = "succeeded",
          bytes = result.bytes,
          warningCodes = result.warningCodes,
        )
      } catch (error: SafDestinationException) {
        androidSafDestinationScript(
          requestId = pending.requestId,
          state = if (error.code == "cancelled") "cancelled" else "failed",
          code = error.code,
          message = error.message,
          warningCodes = error.warningCodes,
        )
      } catch (error: Exception) {
        androidSafDestinationScript(
          requestId = pending.requestId,
          state = "failed",
          code = "destination-write-failed",
          message = "Android SAF destination copy failed",
          warningCodes = listOf(
            "android-saf-provider-not-atomic",
            "partial-destination-may-remain",
          ),
        )
      } finally {
        safDestinationCancellations.remove(pending.requestId, pending.cancellation)
      }
      lifecycleWebView?.evaluateJavascript(script, null)
    }
  }

  private fun injectOpenedFiles(webView: WebView, openedIntent: Intent? = intent) {
    val uris = launchOpenedFileUris(openedIntent)
    if (uris.isEmpty()) return
    safScope.launch {
      val store = SafSpoolStore(File(dataDir, "native-file-jobs/sources"))
      val sources = withContext(Dispatchers.IO) {
        store.cleanupStale()
        uris.map(::contentResolverSource)
      }
      val copyContext = currentCoroutineContext()
      val batch = spoolOpenedFilesOnIo(
        store,
        sources,
        isCancelled = { !copyContext.isActive },
      )
      if (!copyContext.isActive || lifecycleWebView !== webView) return@launch
      webView.evaluateJavascript(androidSpoolBatchScript(batch), null)
    }
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
      sanitizeOpenedFileName(uri.lastPathSegment ?: "opened-file") to null
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
          if (!name.isNullOrBlank()) return sanitizeOpenedFileName(name) to size
        }
      }
    }
    return sanitizeOpenedFileName(uri.lastPathSegment ?: "opened-file") to null
  }
}
