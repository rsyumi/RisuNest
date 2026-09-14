package io.github.rsyumi.risunest

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ExternalStorageAuthorizationTest {
  @Test
  fun redirectsAcceptOnlyRegisteredProviderCallbacks() {
    assertTrue(
      validExternalStorageOAuthRedirect(
        "risunestlocal://oauth/onedrive?code=synthetic&state=opaque",
      ),
    )
    assertTrue(
      validExternalStorageOAuthRedirect(
        "risunestlocal://oauth/google-drive?code=synthetic&state=opaque",
      ),
    )
    assertTrue(
      validExternalStorageOAuthRedirect(
        "risunestlocal://oauth/google-drive?error=access_denied&state=opaque",
      ),
    )
    assertFalse(validExternalStorageOAuthRedirect("risunestlocal://oauth/other?code=x&state=y"))
    assertFalse(validExternalStorageOAuthRedirect("https://oauth/google-drive?code=x&state=y"))
    assertFalse(validExternalStorageOAuthRedirect("risunestlocal://oauth/google-drive#state=y"))
    assertFalse(validExternalStorageOAuthRedirect("risunestlocal://oauth/google-drive"))
    assertFalse(
      validExternalStorageOAuthRedirect(
        "risunestlocal://oauth/google-drive.evil?code=x&state=y",
      ),
    )
  }
}
