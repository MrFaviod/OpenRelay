package dev.openrelay.app

import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties

/**
 * Identidad de cliente en AndroidKeyStore (EC P-256). La clave privada nunca
 * sale del Keystore: Rust sólo pide la clave pública (SPKI DER) y firmas DER
 * ECDSA SHA-256 sobre el mensaje de CertificateVerify de TLS.
 */
class KeystoreSigner {
    private val entry: KeyStore.PrivateKeyEntry

    init {
        val ks = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        if (!ks.containsAlias(ALIAS)) {
            val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN)
                .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
                .setDigests(KeyProperties.DIGEST_SHA256)
                .build()
            KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, ANDROID_KEYSTORE)
                .apply { initialize(spec) }
                .generateKeyPair()
        }
        entry = ks.getEntry(ALIAS, null) as KeyStore.PrivateKeyEntry
    }

    /** SubjectPublicKeyInfo DER de la clave pública (91 bytes para P-256). */
    fun publicKeyDer(): ByteArray = entry.certificate.publicKey.encoded

    /** Firma ECDSA SHA-256 en DER ASN.1, con la clave privada del Keystore. */
    fun sign(message: ByteArray): ByteArray {
        val sig = Signature.getInstance("SHA256withECDSA")
        sig.initSign(entry.privateKey as PrivateKey)
        sig.update(message)
        return sig.sign()
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val ALIAS = "openrelay-device-identity"
    }
}
