import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class PrepareWryRendererRecoveryTest {
    @Test fun clientHookIsIdempotentAndPreservesRequestInterception() {
        val source = """
class RustWebViewClient {
    override fun shouldInterceptRequest(view: WebView, request: Request) = Rust.handleRequest(view, request)
    override fun onPageFinished(view: WebView, url: String) {
        Rust.onPageLoaded(view.id, url)
    }
}
""".trimIndent()
        val patched = patchRendererClient(source)
        assertTrue(patched.contains("@androidx.annotation.RequiresApi(26)"))
        assertTrue(patched.contains("override fun onRenderProcessGone"))
        assertTrue(patched.contains("return host.recoverRenderer(view, detail.didCrash())"))
        assertTrue(patched.contains("Rust.handleRequest(view, request)"))
        assertTrue(patched.contains("Rust.onPageLoaded(view.id, url)"))
        assertEquals(patched, patchRendererClient(patched))
    }

    @Test fun upstreamRendererHandlersAndUnknownTemplatesRequireReview() {
        assertThrows(IllegalStateException::class.java) { patchRendererClient("different template") }
        assertThrows(IllegalStateException::class.java) {
            patchRendererClient("override fun onRenderProcessGone(view: WebView) = true")
        }
    }

    @Test fun deadViewIsReleasedWithoutLosingItsNativeDestroyIdentifier() {
        val source = """
    private lateinit var mWebView: RustWebView
    if (this@WryActivity::mWebView.isInitialized) {
        if (this@WryActivity.mWebView.canGoBack()) {
            this@WryActivity.mWebView.goBack()
        }
    }
        if (::mWebView.isInitialized) {
            mWebView.onPause()
        }
        if (::mWebView.isInitialized) {
            mWebView.onResume()
        }
    Rust.onWebviewDestroy(this, if (::mWebView.isInitialized) { mWebView.id } else { "" })
""".trimStart('\n')
        val patched = patchRendererActivity(source)
        assertTrue(patched.contains("fun releaseFailedWebView(view: WebView)"))
        assertTrue(patched.contains("mWebView?.onPause()"))
        assertTrue(patched.contains("mWebView?.onResume()"))
        assertTrue(patched.contains("Rust.onWebviewDestroy(this, mWebView?.id ?: retiredWebViewId)"))
        assertFalse(patched.contains("::mWebView.isInitialized"))
        assertEquals(patched, patchRendererActivity(patched))
    }
}
