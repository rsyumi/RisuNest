import java.io.File
import org.gradle.api.DefaultTask
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.PathSensitive
import org.gradle.api.tasks.PathSensitivity
import org.gradle.api.tasks.TaskAction

/** Runs after scheduled Rust generators and before Kotlin reads their output. */
open class PrepareWryRendererRecovery : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    lateinit var generatedDirectory: File

    @TaskAction
    fun prepare() {
        val patches = listOf(
            "RustWebViewClient.kt" to ::patchRendererClient,
            "WryActivity.kt" to ::patchRendererActivity,
        )
        // Validate both files before changing either generated source.
        val changes = patches.map { (name, patch) ->
            val file = generatedDirectory.resolve(name)
            val original = file.readText(Charsets.UTF_8)
            Triple(file, original, patch(original))
        }
        changes.forEach { (file, original, patched) ->
            if (original != patched) file.writeText(patched, Charsets.UTF_8)
        }
    }
}

private fun replaceOnce(source: String, from: String, to: String): String {
    check(source.indexOf(from) >= 0 && source.indexOf(from) == source.lastIndexOf(from)) {
        "Wry renderer recovery hook needs review: expected exactly one '$from'"
    }
    return source.replace(from, to)
}

internal fun patchRendererClient(source: String): String {
    val normalized = source.replace("\r\n", "\n")
    val callback = "    override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {"
    val guardedCallback = "    @androidx.annotation.RequiresApi(26)\n$callback"
    if (normalized.contains("// RisuNest renderer recovery callback")) {
        if (normalized.contains(guardedCallback)) return normalized
        return replaceOnce(normalized, callback, guardedCallback)
    }
    check(!normalized.contains("onRenderProcessGone(")) { "Wry already handles renderer exit; review its recovery contract" }
    return replaceOnce(normalized,
        "    override fun onPageFinished(view: WebView, url: String) {",
        """
    // RisuNest renderer recovery callback
    @androidx.annotation.RequiresApi(26)
    override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
        val host = view.context as? RendererRecoveryHost ?: return false
        return host.recoverRenderer(view, detail.didCrash())
    }

    override fun onPageFinished(view: WebView, url: String) {
""".trimStart('\n').trimEnd('\n'))
}

internal fun patchRendererActivity(source: String): String {
    if (source.contains("// RisuNest nullable renderer ownership")) return source
    var result = source.replace("\r\n", "\n")
    result = replaceOnce(result, "    private lateinit var mWebView: RustWebView", """
    // RisuNest nullable renderer ownership
    private var mWebView: RustWebView? = null
    private var retiredWebViewId: String = ""

    fun releaseFailedWebView(view: WebView) {
        if (mWebView === view) {
            retiredWebViewId = mWebView?.id ?: ""
            mWebView = null
        }
    }
""".trimStart('\n').trimEnd('\n'))
    result = replaceOnce(result, "if (this@WryActivity::mWebView.isInitialized)", "if (this@WryActivity.mWebView != null)")
    result = replaceOnce(result, "if (this@WryActivity.mWebView.canGoBack())", "if (this@WryActivity.mWebView?.canGoBack() == true)")
    result = replaceOnce(result, "this@WryActivity.mWebView.goBack()", "this@WryActivity.mWebView?.goBack()")
    for (method in listOf("onPause", "onResume")) {
        result = replaceOnce(result,
            "        if (::mWebView.isInitialized) {\n            mWebView.$method()\n        }",
            "        mWebView?.$method()")
    }
    return replaceOnce(result,
        "if (::mWebView.isInitialized) { mWebView.id } else { \"\" }",
        "mWebView?.id ?: retiredWebViewId")
}
