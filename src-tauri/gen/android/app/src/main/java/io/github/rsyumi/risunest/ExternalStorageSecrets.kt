package io.github.rsyumi.risunest

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Native-only OS protection for external-storage secrets, repository keys and
 * the account token. Each purpose holds its own Keystore key.
 */
internal object ExternalStorageSecrets {
  private const val PROVIDER_PURPOSE = "external-storage-secrets"
  private const val ROOT_KEY_PURPOSE = "external-storage-root-keys"
  private const val ACCOUNT_PURPOSE = "account-credentials"
  private const val PROVIDER_ALIAS = "risunest.external-storage.secrets"
  private const val ROOT_KEY_ALIAS = "risunest.external-storage.root-key"
  private const val ACCOUNT_ALIAS = "risunest.account.credential"
  private const val MAX_ENVELOPE_BYTES = 65_536

  @JvmStatic external fun initialize()

  internal fun alias(purpose: String): String = when (purpose) {
    PROVIDER_PURPOSE -> PROVIDER_ALIAS
    ROOT_KEY_PURPOSE -> ROOT_KEY_ALIAS
    ACCOUNT_PURPOSE -> ACCOUNT_ALIAS
    else -> throw IllegalArgumentException("unsupported external-storage secret purpose")
  }

  @Synchronized
  private fun key(purpose: String): SecretKey {
    val alias = alias(purpose)
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (store.getKey(alias, null) as? SecretKey)?.let { return it }
    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
      init(
        KeyGenParameterSpec.Builder(
          alias,
          KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
          .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
          .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
          .setKeySize(256)
          .build(),
      )
    }.generateKey()
  }

  internal fun validatePlaintextSize(size: Int) {
    require(size in 1..(MAX_ENVELOPE_BYTES - 28))
  }

  internal fun validateEnvelopeSize(size: Int) {
    require(size in 29..MAX_ENVELOPE_BYTES)
  }

  @JvmStatic
  fun seal(purpose: String, input: ByteArray): ByteArray {
    validatePlaintextSize(input.size)
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.ENCRYPT_MODE, key(purpose))
    check(cipher.iv.size == 12)
    cipher.updateAAD(purpose.toByteArray(Charsets.UTF_8))
    return cipher.iv + cipher.doFinal(input)
  }

  @JvmStatic
  fun open(purpose: String, input: ByteArray): ByteArray {
    validateEnvelopeSize(input.size)
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.DECRYPT_MODE, key(purpose), GCMParameterSpec(128, input.copyOfRange(0, 12)))
    cipher.updateAAD(purpose.toByteArray(Charsets.UTF_8))
    return cipher.doFinal(input, 12, input.size - 12)
  }
}
