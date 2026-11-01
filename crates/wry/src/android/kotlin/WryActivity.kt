// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

package {{package}}

import android.annotation.SuppressLint
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.ActivityResult
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner

private const val ACTIVITY_ID_KEY = "__wryActivityId"

object WryLifecycleObserver : DefaultLifecycleObserver {
    // This only runs once: https://developer.android.com/reference/androidx/lifecycle/ProcessLifecycleOwner
    override fun onCreate(owner: LifecycleOwner) {
        super.onCreate(owner)
        Rust.onFirstActivityCreate()
        Rust.onFirstActivityCreateWry()
    }
}

abstract class WryActivity : AppCompatActivity() {
    // RisuNest: the app releases a WebView whose renderer exited, and the
    // native destroy callback still receives that WebView's id.
    private var mWebView: RustWebView? = null
    private var retiredWebViewId: String = ""

    fun releaseFailedWebView(view: WebView) {
        if (mWebView === view) {
            retiredWebViewId = mWebView?.id ?: ""
            mWebView = null
        }
    }

    // RisuNest: while background work runs, the WebView is neither paused nor
    // told that its window is hidden, so the page keeps running off screen.
    private var webViewKeptActive = false

    fun setWebViewKeptActive(kept: Boolean) {
        if (webViewKeptActive == kept) return
        webViewKeptActive = kept
        val webView = mWebView ?: return
        webView.keepWindowVisible = kept
        if (!lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) {
            if (kept) webView.onResume() else webView.onPause()
        }
    }

    private lateinit var permissionLauncher: ActivityResultLauncher<Array<String>>
    private lateinit var activityLauncher: ActivityResultLauncher<Intent>
    private var permissionListener: ((Boolean?) -> Unit)? = null
    private var activityListener: ((ActivityResult?) -> Unit)? = null
    var id: Int = 0
    open val handleBackNavigation: Boolean = true

    open fun onWebViewCreate(webView: WebView) { }

    fun requestPermissions(permissions: Array<String>, listener: (Boolean?) -> Unit) {
        permissionListener = listener
        permissionLauncher.launch(permissions)
    }

    fun launchActivityForResult(intent: Intent, listener: (ActivityResult?) -> Unit) {
        activityListener = listener
        activityLauncher.launch(intent)
    }

    fun setWebView(webView: RustWebView) {
        mWebView = webView
        webView.keepWindowVisible = webViewKeptActive

        if (handleBackNavigation) {
            val callback = object : OnBackPressedCallback(true) {
                override fun handleOnBackPressed() {
                    if (this@WryActivity.mWebView != null) {
                        if (this@WryActivity.mWebView?.canGoBack() == true) {
                            this@WryActivity.mWebView?.goBack()
                        } else {
                            this.isEnabled = false
                            this@WryActivity.onBackPressed()
                            this.isEnabled = true
                        }
                    }
                }
            }
            onBackPressedDispatcher.addCallback(this, callback)
        }

        onWebViewCreate(webView)
    }

    val version: String
        @SuppressLint("WebViewApiAvailability", "ObsoleteSdkInt")
        get() {
            // Check getCurrentWebViewPackage() directly if above Android 8
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                return WebView.getCurrentWebViewPackage()?.versionName ?: ""
            }

            // Otherwise manually check WebView versions
            var webViewPackage = "com.google.android.webview"
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
              webViewPackage = "com.android.chrome"
            }
            try {
                @Suppress("DEPRECATION")
                val info = packageManager.getPackageInfo(webViewPackage, 0)
                return info.versionName.toString()
            } catch (ex: Exception) {
                Logger.warn("Unable to get package info for '$webViewPackage'$ex")
            }

            try {
                @Suppress("DEPRECATION")
                val info = packageManager.getPackageInfo("com.android.webview", 0)
                return info.versionName.toString()
            } catch (ex: Exception) {
                Logger.warn("Unable to get package info for 'com.android.webview'$ex")
            }

            // Could not detect any webview, return empty string
            return ""
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        id = savedInstanceState?.getInt(ACTIVITY_ID_KEY) ?: intent.extras?.getInt(ACTIVITY_ID_KEY) ?: hashCode()

        permissionLauncher = registerForActivityResult(
            ActivityResultContracts.RequestMultiplePermissions(),
        ) { isGranted ->
            permissionListener?.let { listener ->
                val allGranted = isGranted.values.all { it }
                listener(allGranted)
            }
        }
        activityLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            activityListener?.invoke(result)
        }

        Rust.onCreate(this)
        ProcessLifecycleOwner.get().lifecycle.addObserver(WryLifecycleObserver)
    }

    override fun onStart() {
        super.onStart()
        Rust.onStart(this)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        Rust.onWindowFocusChanged(this, hasFocus)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putInt(ACTIVITY_ID_KEY, id)
    }

    override fun onPause() {
        super.onPause()
        Rust.onPause(this)
        if (!webViewKeptActive) mWebView?.onPause()
    }

    override fun onResume() {
        super.onResume()
        Rust.onResume(this)
        mWebView?.onResume()
    }

    override fun onStop() {
        super.onStop()
        Rust.onStop(this)
    }

    override fun onDestroy() {
        super.onDestroy()
        Rust.onDestroy(this)
        Rust.onWebviewDestroy(this, mWebView?.id ?: retiredWebViewId)
    }

    override fun onLowMemory() {
        super.onLowMemory()
        Rust.onLowMemory()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        Rust.onNewIntent(intent)
    }

    fun getAppClass(name: String): Class<*> {
        return Class.forName(name)
    }

    // Called by tao through JNI
    fun startActivity(cls: Class<*>): Int {
        val intent = Intent(this, cls)
        val id = kotlin.random.Random.nextInt()
        intent.putExtra(ACTIVITY_ID_KEY, id)
        startActivity(intent)
        return id
    }

    {{class-extension}}
}
