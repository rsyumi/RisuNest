package io.github.rsyumi.risunest

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.lang.reflect.Modifier

class ExternalStorageSecretsTest {
  @Test
  fun nativeEntryPointsMatchTheJniSignatures() {
    val type = ExternalStorageSecrets::class.java
    for (name in listOf("seal", "open")) {
      val method = type.getDeclaredMethod(name, String::class.java, ByteArray::class.java)
      assertTrue(Modifier.isPublic(method.modifiers))
      assertTrue(Modifier.isStatic(method.modifiers))
      assertEquals(ByteArray::class.java, method.returnType)
    }
    val initialize = type.getDeclaredMethod("initialize")
    assertTrue(Modifier.isStatic(initialize.modifiers))
    assertTrue(Modifier.isNative(initialize.modifiers))
    assertEquals(Void.TYPE, initialize.returnType)
  }

  @Test
  fun everyNativePurposeMapsToItsOwnKeystoreAlias() {
    val aliases = listOf(
      "external-storage-secrets",
      "external-storage-root-keys",
      "account-credentials",
    ).map(ExternalStorageSecrets::alias)

    assertEquals(aliases.size, aliases.toSet().size)
    assertThrows(IllegalArgumentException::class.java) {
      ExternalStorageSecrets.alias("account-credentials-2")
    }
  }

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
