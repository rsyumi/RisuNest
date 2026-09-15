package io.github.rsyumi.risunest

import android.content.Intent
import java.net.URI

private const val ONEDRIVE_REDIRECT_URI = "risunestlocal://oauth/onedrive"
private const val GOOGLE_DRIVE_REDIRECT_URI = "risunestlocal://oauth/google-drive"

internal fun validExternalStorageOAuthRedirect(raw: String): Boolean = runCatching {
  if (raw.length !in 1..16_384) return@runCatching false
  val uri = URI(raw)
  val expected = when (uri.path) {
    "/onedrive" -> ONEDRIVE_REDIRECT_URI
    "/google-drive" -> GOOGLE_DRIVE_REDIRECT_URI
    else -> return@runCatching false
  }
  uri.scheme == "risunestlocal" &&
    uri.host == "oauth" &&
    uri.port == -1 &&
    uri.userInfo == null &&
    uri.fragment == null &&
    !uri.rawQuery.isNullOrEmpty() &&
    raw.startsWith("$expected?")
}.getOrDefault(false)

/** Delivers browser OAuth callbacks directly to the native Rust pending flow. */
internal object ExternalStorageAuthorization {
  @JvmStatic
  private external fun completeOAuthRedirectNative(redirectUrl: String)

  fun onOAuthRedirectIntent(intent: Intent?): Boolean {
    if (intent?.action != Intent.ACTION_VIEW) return false
    val redirect = intent.dataString ?: return false
    if (!validExternalStorageOAuthRedirect(redirect)) return false
    completeOAuthRedirectNative(redirect)
    return true
  }
}
