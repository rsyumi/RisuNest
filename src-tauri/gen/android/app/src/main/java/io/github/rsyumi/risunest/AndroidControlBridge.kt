package io.github.rsyumi.risunest

import android.webkit.WebView
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import org.json.JSONObject
import java.net.URI

internal val CONTROL_ORIGINS = setOf("http://tauri.localhost", "https://tauri.localhost")

internal fun androidControlOrigins(debug: Boolean, developmentOrigin: String): Set<String> {
  if (!debug || developmentOrigin.isEmpty()) return CONTROL_ORIGINS
  val uri = runCatching { URI(developmentOrigin) }.getOrNull() ?: return CONTROL_ORIGINS
  if (uri.scheme !in setOf("http", "https") || uri.host.isNullOrEmpty() ||
    uri.userInfo != null || !uri.rawPath.isNullOrEmpty() || uri.rawQuery != null ||
    uri.rawFragment != null || uri.port < -1 || uri.port > 65535) return CONTROL_ORIGINS
  return CONTROL_ORIGINS + developmentOrigin
}

internal fun acceptsAndroidControl(
  origin: String,
  mainFrame: Boolean,
  allowedOrigins: Set<String> = CONTROL_ORIGINS,
): Boolean = mainFrame && origin in allowedOrigins

internal fun androidControlNotification(method: String): Boolean =
  method.startsWith("lifecycle.") || method in setOf(
    "saf.copyExport", "saf.cancelSource", "saf.pickBackupSource",
    "saf.pickLegacyBackupSource", "saf.pickContentSource",
  )

internal fun androidControlArgumentCount(method: String): Int? = when (method) {
  "lifecycle.onFlushComplete", "lifecycle.onFlushHold",
  "saf.pickBackupSource", "saf.pickLegacyBackupSource", "saf.cancelExport",
  "saf.cancelSource", "saf.discardSource", "saf.markExportPublicationReady",
  "saf.acknowledgeExport" -> 1
  "saf.pickContentSource" -> 2
  "saf.copyExport" -> 3
  "lifecycle.requestExit", "lifecycle.requestRestart", "lifecycle.onFrontendReady",
  "generation.begin", "generation.end", "generation.notificationsEnabled", "generation.requestNotifications",
  "generation.openNotificationSettings", "generation.webViewVersion",
  "saf.getActiveSourceRequestIds", "saf.getExportStatus", "saf.getExportSourceId" -> 0
  else -> null
}

/** Control messages carry no file bytes and never accept requests from plugin frames. */
internal class AndroidControlBridge private constructor(
  private val view: WebView,
  private val name: String,
  private val dispatch: suspend (String, List<String>) -> Any?,
) : AutoCloseable {
  private val origins = androidControlOrigins(BuildConfig.DEBUG, BuildConfig.CONTROL_DEV_ORIGIN)
  private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
  private var closed = false
  private var pending = 0

  private fun register() {
    WebViewCompat.addWebMessageListener(view, name, origins) { _, message, origin, mainFrame, reply ->
      if (closed || !acceptsAndroidControl(origin.toString(), mainFrame, origins) ||
        message.type != WebMessageCompat.TYPE_STRING) return@addWebMessageListener
      val text = message.data ?: return@addWebMessageListener
      if (text.length > 32 * 1024) return@addWebMessageListener
      val request = runCatching { JSONObject(text) }.getOrNull() ?: return@addWebMessageListener
      val method = request.optString("method")
      if (method.startsWith("saf.") != (name == "RisuNestSafControl")) return@addWebMessageListener
      val argumentCount = androidControlArgumentCount(method) ?: return@addWebMessageListener
      val id = request.optString("id").takeIf { isCanonicalUuidV4(it) }
      if (id == null && !androidControlNotification(method)) return@addWebMessageListener
      val values = request.optJSONArray("args") ?: return@addWebMessageListener
      if (values.length() != argumentCount) return@addWebMessageListener
      val args = (0 until values.length()).map {
        values.opt(it) as? String ?: return@addWebMessageListener
      }
      if (pending >= 32 && id != null) {
        reply.postMessage(JSONObject().put("id", id).put("error", "android-control-busy").toString())
        return@addWebMessageListener
      }
      pending++
      scope.launch {
        try {
          val result = runCatching { dispatch(method, args) }
          if (!closed && id != null) {
            val response = JSONObject().put("id", id)
            result.fold(
              onSuccess = { response.put("result", if (it == null || it == Unit) JSONObject.NULL else it) },
              onFailure = { response.put("error", "android-control-failed") },
            )
            // The proxy replies only to the document that made this request.
            runCatching { reply.postMessage(response.toString()) }
          }
        } finally {
          pending--
        }
      }
    }
  }

  override fun close() {
    if (closed) return
    closed = true
    runCatching { WebViewCompat.removeWebMessageListener(view, name) }
    scope.cancel()
  }

  companion object {
    private const val NAME = "RisuNestControl"

    fun attach(
      view: WebView,
      dispatch: suspend (String, List<String>) -> Any?,
      name: String = NAME,
    ): AndroidControlBridge? {
      if (!runCatching {
        WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)
      }.getOrDefault(false)) return null
      val bridge = AndroidControlBridge(view, name, dispatch)
      return try { bridge.register(); bridge } catch (_: Exception) { bridge.close(); null }
    }
  }
}
