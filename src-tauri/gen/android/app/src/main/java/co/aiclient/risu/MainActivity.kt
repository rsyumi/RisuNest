package co.aiclient.risu

import android.content.ComponentCallbacks2
import android.os.Bundle
import android.os.Process
import android.os.SystemClock
import android.view.ViewGroup
import android.webkit.WebView
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updateLayoutParams

private const val EXIT_CONFIRMATION_WINDOW_MILLIS = 2_000L
private const val NATIVE_LIFECYCLE_EVENT = "risu-native-lifecycle"
private const val STOP_REASON = "stop"
private const val TRIM_MEMORY_REASON = "trim-memory"

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

internal fun exitApplication(removeTask: () -> Unit, terminateProcess: () -> Unit) {
  removeTask()
  terminateProcess()
}

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

class MainActivity : TauriActivity() {
  private val backNavigationPolicy = BackNavigationPolicy()
  private var lifecycleWebView: WebView? = null
  private val lifecycleFlushDispatcher = LifecycleFlushDispatcher(::dispatchLifecycleFlush)

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    lifecycleWebView = webView

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
            BackNavigationAction.SHOW_EXIT_HINT -> Toast.makeText(
              this@MainActivity,
              R.string.press_back_again_to_exit,
              Toast.LENGTH_SHORT,
            ).show()
            BackNavigationAction.EXIT -> exitApplication(
              removeTask = ::finishAndRemoveTask,
              terminateProcess = { Process.killProcess(Process.myPid()) },
            )
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
}
