# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# Kotlin symbols that src-tauri Rust code looks up by name over JNI
# (JNIEnv::call_method / GetMethodID) need explicit keep rules: R8 renaming or
# stripping them breaks the callback silently, because the native side clears
# the pending exception and carries on (src-tauri/src/peer_sync/android_jni.rs).
# Keep this list in sync with every call_method/find_class in src-tauri Rust.
# The JNI entry points themselves (the `external fun` declarations on
# PeerCloneNativeBridge and PeerSyncForegroundNativeBridge) are already kept by
# the generated proguard-wry.pro `native <methods>` rule.
-keep interface co.aiclient.risu.PeerCloneNativeProgress { *; }
-keepclassmembers class * implements co.aiclient.risu.PeerCloneNativeProgress {
    void onProgress(long);
}
