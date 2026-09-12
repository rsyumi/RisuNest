package io.github.rsyumi.risunest

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Called only through native JNI, never exposed as a WebView Javascript bridge. */
internal object ServerSyncSecrets {
  private const val ALIAS = "risunest.server-sync.device-token"

  @JvmStatic external fun initialize()

  @Synchronized
  private fun key(): SecretKey {
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (store.getKey(ALIAS, null) as? SecretKey)?.let { return it }
    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
      init(KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .build())
    }.generateKey()
  }

  @JvmStatic
  fun seal(input: ByteArray): ByteArray {
    require(input.size == 64)
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.ENCRYPT_MODE, key())
    check(cipher.iv.size == 12)
    return cipher.iv + cipher.doFinal(input)
  }

  @JvmStatic
  fun open(input: ByteArray): ByteArray {
    require(input.size == 92)
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, input.copyOfRange(0, 12)))
    return cipher.doFinal(input, 12, input.size - 12)
  }
}
