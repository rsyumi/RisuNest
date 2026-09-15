package io.github.rsyumi.risunest

import org.junit.Assert.assertThrows
import org.junit.Test

class ExternalStorageSecretsTest {
  @Test
  fun plaintextAndEnvelopeBoundsRejectEmptyAndOversizedValues() {
    ExternalStorageSecrets.validatePlaintextSize(1)
    ExternalStorageSecrets.validatePlaintextSize(65_508)
    assertThrows(IllegalArgumentException::class.java) {
      ExternalStorageSecrets.validatePlaintextSize(0)
    }
    assertThrows(IllegalArgumentException::class.java) {
      ExternalStorageSecrets.validatePlaintextSize(65_509)
    }

    ExternalStorageSecrets.validateEnvelopeSize(29)
    ExternalStorageSecrets.validateEnvelopeSize(65_536)
    assertThrows(IllegalArgumentException::class.java) {
      ExternalStorageSecrets.validateEnvelopeSize(28)
    }
    assertThrows(IllegalArgumentException::class.java) {
      ExternalStorageSecrets.validateEnvelopeSize(65_537)
    }
  }
}
