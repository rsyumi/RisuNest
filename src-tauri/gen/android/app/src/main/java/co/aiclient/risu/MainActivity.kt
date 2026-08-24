package co.aiclient.risu

import android.content.ComponentCallbacks2
import android.content.ContentResolver
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.OpenableColumns
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
) {
  fun restart() {
    relaunchTask()
    terminateProcess()
  }
}

class MainActivity : TauriActivity() {
  private val backNavigationPolicy = BackNavigationPolicy()
  private var lifecycleWebView: WebView? = null
  private val lifecycleFlushDispatcher = LifecycleFlushDispatcher(::dispatchLifecycleFlush)
  private val exitFlushGate = ExitFlushGate()
  private val mainHandler = Handler(Looper.getMainLooper())
  private val coldRestartDispatcher by lazy {
    ColdRestartDispatcher(
      relaunchTask = {
        startActivity(Intent.makeRestartActivityTask(componentName))
      },
      terminateProcess = {
        android.os.Process.killProcess(android.os.Process.myPid())
      },
    )
  }
  private var exitFlushSequence = 0L

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
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
