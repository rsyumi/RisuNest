package io.github.rsyumi.risunest

import org.junit.Assert.assertThrows
import org.junit.Test

class ServerSyncSecretsTest {
  @Test fun acceptsStructuredCredentialsBeyondTheOldTokenLength() {
    ServerSyncSecrets.validatePlaintextSize(2048)
    ServerSyncSecrets.validateEnvelopeSize(2076)
    ServerSyncSecrets.validatePlaintextSize(16356)
    ServerSyncSecrets.validateEnvelopeSize(16384)
  }

  @Test fun rejectsEmptyTruncatedAndOversizedCredentials() {
    for (size in listOf(0, 16357)) {
      assertThrows(IllegalArgumentException::class.java) {
        ServerSyncSecrets.validatePlaintextSize(size)
      }
    }
    for (size in listOf(0, 28, 16385)) {
      assertThrows(IllegalArgumentException::class.java) {
        ServerSyncSecrets.validateEnvelopeSize(size)
      }
    }
  }
}
