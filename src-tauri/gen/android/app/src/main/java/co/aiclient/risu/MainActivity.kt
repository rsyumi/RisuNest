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
import androidx.core.content.IntentCompat
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updateLayoutParams
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.io.File

private const val EXIT_CONFIRMATION_WINDOW_MILLIS = 2_000L
private const val EXIT_FLUSH_TIMEOUT_MILLIS = 1_500L
private const val NATIVE_LIFECYCLE_EVENT = "risu-native-lifecycle"
private const val LIFECYCLE_BRIDGE_NAME = "RisuLifecycleBridge"
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
      removeJavascriptBridge = { webView.removeJavascriptInterface(LIFECYCLE_BRIDGE_NAME) },
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
    injectOpenedFiles(webView)

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

  private fun injectOpenedFiles(webView: WebView) {
    val openedFiles = copyOpenedFiles(launchOpenedFileUris(intent))
    if (openedFiles.isEmpty()) {
      return
    }
    val script = openedFilesScript(openedFiles)
    if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
      WebViewCompat.addDocumentStartJavaScript(webView, script, setOf("*"))
    } else {
      webView.evaluateJavascript(script, null)
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

  private fun copyOpenedFiles(uris: List<Uri>): List<String> {
    if (uris.isEmpty()) {
      return emptyList()
    }
    val directory = File(cacheDir, "opened_files")
    directory.mkdirs()
    val stamp = System.currentTimeMillis()
    return uris.mapIndexedNotNull { index, uri ->
      try {
        val target = File(directory, "$stamp-$index-${resolveDisplayName(uri)}")
        contentResolver.openInputStream(uri)?.use { input ->
          target.outputStream().use { output -> input.copyTo(output) }
        } ?: return@mapIndexedNotNull null
        target.absolutePath
      } catch (error: Exception) {
        null
      }
    }
  }

  private fun resolveDisplayName(uri: Uri): String {
    if (uri.scheme == ContentResolver.SCHEME_CONTENT) {
      contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
        val column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
        if (column >= 0 && cursor.moveToFirst()) {
          val name = cursor.getString(column)
          if (!name.isNullOrBlank()) {
            return sanitizeOpenedFileName(name)
          }
        }
      }
    }
    return sanitizeOpenedFileName(uri.lastPathSegment ?: "opened-file")
  }
}
