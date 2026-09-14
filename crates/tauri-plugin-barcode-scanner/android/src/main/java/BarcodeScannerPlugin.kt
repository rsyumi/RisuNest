// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

package app.tauri.barcodescanner

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.content.Context.MODE_PRIVATE
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.drawable.Drawable
import android.net.Uri
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.provider.Settings
import android.util.Size
import android.view.ViewGroup
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.result.ActivityResult
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import androidx.lifecycle.LifecycleOwner
import app.tauri.Logger
import app.tauri.PermissionState
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.google.common.util.concurrent.ListenableFuture
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import java.util.Collections
import java.util.concurrent.ExecutionException

private const val PERMISSION_ALIAS_CAMERA = "camera"
private const val PERMISSION_NAME = Manifest.permission.CAMERA

@InvokeArg
class ScanOptions {
    var formats: Array<String>? = null
    var windowed: Boolean = false
    var cameraDirection: String? = null
}

@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.CAMERA], alias = "camera")
    ]
)
class BarcodeScannerPlugin(private val activity: Activity) : Plugin(activity),
    ImageAnalysis.Analyzer {
    private lateinit var webView: WebView
    private var previewView: PreviewView? = null
    private var cameraProviderFuture: ListenableFuture<ProcessCameraProvider>? = null
    private var cameraProvider: ProcessCameraProvider? = null
    private var graphicOverlay: GraphicOverlay? = null
    private var camera: Camera? = null
    private var vibrator: Vibrator? = null

    private var scannerOptions: BarcodeScannerOptions? = null
    private var scanner: com.google.mlkit.vision.barcode.BarcodeScanner? = null

    private var requestPermissionResponse: JSObject? = null
    private var windowed = false

    // declare a map constant for allowed barcode formats
    private val supportedFormats = supportedFormats()

    private val scanSession = ScanSession<Invoke>()
    private val lifecycleObserver = ScanLifecycleObserver {
        if (::webView.isInitialized && scanSession.pending != null) {
            failScan("qr-scan-cancelled")
        }
    }
    private var lifecycleOwner: LifecycleOwner? = null
    private var webViewBackground: Drawable? = null

    override fun load(webView: WebView) {
        super.load(webView)
        this.webView = webView
        lifecycleOwner = activity as? LifecycleOwner
        lifecycleOwner?.lifecycle?.addObserver(lifecycleObserver)
    }

    private fun supportedFormats(): Map<String, Int> {
        val map: MutableMap<String, Int> = HashMap()
        map["UPC_A"] = Barcode.FORMAT_UPC_A
        map["UPC_E"] = Barcode.FORMAT_UPC_E
        map["EAN_8"] = Barcode.FORMAT_EAN_8
        map["EAN_13"] = Barcode.FORMAT_EAN_13
        map["CODE_39"] = Barcode.FORMAT_CODE_39
        map["CODE_93"] = Barcode.FORMAT_CODE_93
        map["CODE_128"] = Barcode.FORMAT_CODE_128
        map["CODABAR"] = Barcode.FORMAT_CODABAR
        map["ITF"] = Barcode.FORMAT_ITF
        map["AZTEC"] = Barcode.FORMAT_AZTEC
        map["DATA_MATRIX"] = Barcode.FORMAT_DATA_MATRIX
        map["PDF_417"] = Barcode.FORMAT_PDF417
        map["QR_CODE"] = Barcode.FORMAT_QR_CODE
        return Collections.unmodifiableMap(map)
    }

    override fun onStop() {
        if (::webView.isInitialized && scanSession.pending != null) failScan("qr-scan-cancelled")
    }

    override fun onDestroy(activity: androidx.appcompat.app.AppCompatActivity) {
        lifecycleOwner?.lifecycle?.removeObserver(lifecycleObserver)
        lifecycleOwner = null
        if (::webView.isInitialized) failScan("qr-scan-cancelled")
    }

    private fun hasCamera(): Boolean {
        return activity.packageManager
            .hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)
    }

    private fun setupCamera(cameraDirection: String, windowed: Boolean) {
        val generation = scanSession.generation
        activity
            .runOnUiThread {
                if (!scanSession.isCurrent(generation)) return@runOnUiThread
                val previewView = PreviewView(activity)
                previewView.layoutParams = FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT
                )
                this.previewView = previewView

                val graphicOverlay = GraphicOverlay(activity)
                graphicOverlay.layoutParams = FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT
                )
                this.graphicOverlay = graphicOverlay

                val parent = webView.parent as ViewGroup
                parent.addView(previewView)
                parent.addView(graphicOverlay)

                this.windowed = windowed
                if (windowed) {
                    webView.bringToFront()
                    webViewBackground = webView.background
                    webView.setBackgroundColor(Color.TRANSPARENT)
                }

                val cameraProviderFuture = ProcessCameraProvider.getInstance(activity)
                cameraProviderFuture.addListener(
                    {
                        try {
                            val cameraProvider = cameraProviderFuture.get()
                            if (!scanSession.isCurrent(generation)) return@addListener
                            this.cameraProvider = cameraProvider
                            bindPreview(
                                cameraProvider,
                                if (cameraDirection == "front") CameraSelector.LENS_FACING_FRONT else CameraSelector.LENS_FACING_BACK
                            )
                        } catch (_: Exception) {
                            if (generation == scanSession.generation) failScan("qr-camera-unavailable")
                        }
                    },
                    ContextCompat.getMainExecutor(activity)
                )
                this.cameraProviderFuture = cameraProviderFuture
            }
    }

    private fun bindPreview(cameraProvider: ProcessCameraProvider, cameraDirection: Int) {
        activity
            .runOnUiThread {
                val preview = Preview.Builder().build()
                val cameraSelector =
                    CameraSelector.Builder().requireLensFacing(cameraDirection).build()
                preview.setSurfaceProvider(previewView?.surfaceProvider)
                val imageAnalysis = ImageAnalysis.Builder()
                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                    .setTargetResolution(Size(1280, 720))
                    .build()
                imageAnalysis.setAnalyzer(
                    ContextCompat.getMainExecutor(activity),
                    this
                )

                try {
                    camera = cameraProvider.bindToLifecycle(
                        activity as LifecycleOwner,
                        cameraSelector,
                        preview,
                        imageAnalysis
                    )
                } catch (_: Exception) {
                    failScan("qr-camera-unavailable")
                }
            }
    }

    private fun dismantleCamera() {
        cameraProvider?.unbindAll()
        cameraProvider = null
        cameraProviderFuture = null
        val parent = webView.parent as ViewGroup
        previewView?.let { parent.removeView(it) }
        graphicOverlay?.let { parent.removeView(it) }
        camera = null
        previewView = null
        graphicOverlay = null
        scanner?.close()
        scanner = null
    }

    private fun failScan(code: String) {
        val pending = scanSession.finish()
        destroy()
        pending?.reject(code)
    }

    private fun getFormats(args: ScanOptions): List<Int> {
        val formats = ArrayList<Int>()
        for (format in args.formats ?: arrayOf()) {
            val targetedBarcodeFormat = supportedFormats[format]
            if (targetedBarcodeFormat != null) {
                formats.add(targetedBarcodeFormat)
            }
        }
        return formats
    }

    private fun prepare(direction: String, windowed: Boolean) {
        dismantleCamera()
        setupCamera(direction, windowed)
    }

    private fun destroy() {
        scanSession.finish()
        dismantleCamera()
        if (windowed) {
            if (webViewBackground != null) {
                webView.background = webViewBackground
                webViewBackground = null
            } else {
                webView.setBackgroundColor(Color.WHITE)
            }
        }
        windowed = false
    }

    @Suppress("DEPRECATION")
    private fun configureCamera(formats: List<Int>) {
        activity
            .runOnUiThread {
                if (scanSession.pending == null || previewView == null) return@runOnUiThread
                val vibrator =
                    activity.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
                this.vibrator = vibrator
                if (previewView == null) {
                    throw Exception("Something went wrong configuring the BarcodeScanner")
                }

                if (formats.isNotEmpty()) {
                    val mappedFormats = mapFormats(formats)
                    val options =
                        BarcodeScannerOptions.Builder()
                            .setBarcodeFormats(Barcode.FORMAT_QR_CODE, *mappedFormats).build()
                    scannerOptions = options
                    scanner = BarcodeScanning.getClient(options)
                } else {
                    val options = BarcodeScannerOptions.Builder()
                        .setBarcodeFormats(Barcode.FORMAT_ALL_FORMATS).build()
                    scannerOptions = options
                    scanner = BarcodeScanning.getClient(options)
                }
            }
    }

    private fun mapFormats(integers: List<Int>): IntArray {
        val ret = IntArray(integers.size)
        for (i in ret.indices) {
            if (integers[i] != Barcode.FORMAT_QR_CODE) ret[i] = integers[i]
        }
        return ret
    }

    override fun analyze(image: ImageProxy) {
        @SuppressLint("UnsafeOptInUsageError") val mediaImage = image.image
        val generation = scanSession.generation
        if (mediaImage != null && scanner != null) {
            val inputImage =
                InputImage.fromMediaImage(mediaImage, image.imageInfo.rotationDegrees)
            scanner
                ?.process(inputImage)
                ?.addOnSuccessListener { barcodes ->
                    scanSession.recordFrameSuccess(generation)
                    if (scanSession.isCurrent(generation) && barcodes.isNotEmpty()) {
                        val barcode = barcodes[0]
                        val bounds = barcode.boundingBox
                        val rawValue = barcode.rawValue ?: ""
                        val rawFormat = barcode.format
                        var format: String? = null

                        for (entry in supportedFormats.entries) {
                            if (entry.value == rawFormat) {
                                format = entry.key
                                break
                            }
                        }

                        val s = bounds?.flattenToString()
                        val jsObject = JSObject()
                        jsObject.put("content", rawValue)
                        jsObject.put("format", format)
                        jsObject.put("bounds", s)

                        scanSession.pending?.resolve(jsObject)
                        destroy()
                    }
                }
                ?.addOnFailureListener { error ->
                    Logger.error(error.message ?: error.toString())
                    if (scanSession.shouldFailAfterFrameError(generation)) {
                        failScan("qr-camera-unavailable")
                    }
                }
                ?.addOnCompleteListener {
                    image.close()
                    // ImageProxy.close owns the underlying media image.
                }
        } else { image.close() }
    }

    @Command
    fun vibrate(invoke: Invoke) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            vibrator!!.vibrate(
                VibrationEffect.createOneShot(
                    50,
                    VibrationEffect.DEFAULT_AMPLITUDE
                )
            )
        }
        invoke.resolve()
    }

    @Command
    fun cancel(invoke: Invoke) {
        activity.runOnUiThread {
            failScan("qr-scan-cancelled")
            invoke.resolve()
        }
    }

    @Command
    fun scan(invoke: Invoke) {
        val args = invoke.parseArgs(ScanOptions::class.java)
        activity.runOnUiThread {
            if (scanSession.pending != null) { invoke.reject("qr-scan-busy"); return@runOnUiThread }
            if (!hasCamera()) { invoke.reject("qr-camera-unavailable"); return@runOnUiThread }
            if (getPermissionState("camera") != PermissionState.GRANTED) { invoke.reject("qr-camera-permission-denied"); return@runOnUiThread }
            scanSession.begin(invoke)
            webViewBackground = null
            prepare(args.cameraDirection ?: "back", args.windowed)
            configureCamera(getFormats(args))
        }
    }

    @SuppressLint("ObsoleteSdkInt")
    @PermissionCallback
    fun cameraPermissionCallback(invoke: Invoke) {
        if (requestPermissionResponse == null) {
            return
        }

        val requestPermissionResponse = requestPermissionResponse!!

        val granted = getPermissionState(PERMISSION_ALIAS_CAMERA) === PermissionState.GRANTED

        if (granted) {
            requestPermissionResponse.put(PERMISSION_ALIAS_CAMERA, PermissionState.GRANTED)
        } else {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                requestPermissionResponse.put(PERMISSION_ALIAS_CAMERA, PermissionState.DENIED)
            } else {
                requestPermissionResponse.put(PERMISSION_ALIAS_CAMERA, PermissionState.GRANTED)
            }
        }

        invoke.resolve(requestPermissionResponse)
        this.requestPermissionResponse = null
    }

    @SuppressLint("ObsoleteSdkInt")
    @Command
    override fun requestPermissions(invoke: Invoke) {
        val requestPermissionResponse = JSObject()
        this.requestPermissionResponse = requestPermissionResponse
        if (getPermissionState(PERMISSION_ALIAS_CAMERA) === PermissionState.GRANTED) {
            requestPermissionResponse.put(PERMISSION_ALIAS_CAMERA, PermissionState.GRANTED)
        } else {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                requestPermissionForAlias(
                    PERMISSION_ALIAS_CAMERA,
                    invoke,
                    "cameraPermissionCallback"
                )
                return
            } else {
                requestPermissionResponse.put(PERMISSION_ALIAS_CAMERA, PermissionState.GRANTED)
            }
        }
        invoke.resolve(requestPermissionResponse)
    }

    @Command
    fun openAppSettings(invoke: Invoke) {
        val intent = Intent(
            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            Uri.fromParts("package", activity.packageName, null)
        )
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivityForResult(invoke, intent, "openSettingsResult")
    }

    @ActivityCallback
    private fun openSettingsResult(invoke: Invoke, result: ActivityResult) {
        invoke.resolve()
    }
}
