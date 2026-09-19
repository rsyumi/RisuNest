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

# Native credential access uses this class by name through JNI.
-keep class io.github.rsyumi.risunest.ServerSyncSecrets { *; }

# Rust invokes these static entry points by name; keep their JNI signatures.
-keep class io.github.rsyumi.risunest.ExternalStorageSecrets {
    public static native void initialize();
    public static byte[] seal(java.lang.String, byte[]);
    public static byte[] open(java.lang.String, byte[]);
}
