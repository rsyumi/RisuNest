package io.github.rsyumi.risunest

import android.webkit.WebView
import androidx.annotation.Keep
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

@Keep
internal object AndroidCommitNative {
  external fun append(packet: ByteArray): Int
  external fun reset()
}

internal fun supportsBinaryCommit(listener: Boolean, arrayBuffer: Boolean) = listener && arrayBuffer

internal val COMMIT_ORIGINS = setOf("http://tauri.localhost", "https://tauri.localhost")
internal fun acceptsBinaryCommit(origin: String, mainFrame: Boolean) = mainFrame && origin in COMMIT_ORIGINS

/** Only the app's main document can submit chunks. Durable finish stays in Tauri. */
internal class AndroidCommitBridge private constructor(private val view: WebView) : AutoCloseable {
  private val executor = Executors.newSingleThreadExecutor()
  private val busy = AtomicBoolean(false)
  private val closed = AtomicBoolean(false)

  private fun register() {
    WebViewCompat.addWebMessageListener(view, NAME, COMMIT_ORIGINS) { _, message, origin, mainFrame, reply ->
      if (closed.get() || !acceptsBinaryCommit(origin.toString(), mainFrame) ||
        message.type != WebMessageCompat.TYPE_ARRAY_BUFFER) return@addWebMessageListener
      // Admission happens before a worker task is queued. At most one bounded packet is retained.
      if (!busy.compareAndSet(false, true)) return@addWebMessageListener
      val packet = message.arrayBuffer
      if (packet.size <= 40 || packet.size > 40 + 256 * 1024) {
        busy.set(false)
        return@addWebMessageListener
      }
      val id = String(packet, 0, 36, Charsets.US_ASCII)
      executor.execute {
        val offset = if (closed.get()) -1 else runCatching { AndroidCommitNative.append(packet) }.getOrDefault(-1)
        view.post {
          busy.set(false)
          if (!closed.get()) {
            val response = JSONObject().put("id", id)
            if (offset < 0) response.put("error", "invalid-chunk") else response.put("offset", offset)
            // Reply proxy belongs to the originating document, not a replacement page.
            runCatching { reply.postMessage(response.toString()) }
          }
        }
      }
    }
  }

  override fun close() {
    if (!closed.compareAndSet(false, true)) return
    runCatching { WebViewCompat.removeWebMessageListener(view, NAME) }
    executor.shutdownNow()
    runCatching { AndroidCommitNative.reset() }
  }

  companion object {
    private const val NAME = "RisuNestCommit"
    fun attach(view: WebView): AndroidCommitBridge? {
      val supported = runCatching {
        supportsBinaryCommit(
          WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER),
          WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_ARRAY_BUFFER),
        )
      }.getOrDefault(false)
      if (!supported) return null
      val bridge = AndroidCommitBridge(view)
      return try { bridge.register(); bridge } catch (_: Exception) { bridge.close(); null }
    }
  }
}
