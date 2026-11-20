package dev.openrelay.app

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Handshake mTLS real con la clave de AndroidKeyStore. El puente Rust
 * (libopenrelay_bridge.so) firma por JNI con KeystoreSigner; el informe empieza por
 * "OK" sólo si el servidor autorizó y el Keystore firmó al menos una vez.
 */
@RunWith(AndroidJUnit4::class)
class KeystoreHandshakeTest {

    @Test
    fun keystoreKeyAuthorizesMtlsSession() {
        val report = Companion.nativeHandshake(KeystoreSigner())
        println("KEYSTORE_HANDSHAKE_REPORT $report")
        assertTrue(report, report.startsWith("OK "))
    }

    companion object {
        init {
            System.loadLibrary("openrelay_bridge")
        }

        @JvmStatic
        private external fun nativeHandshake(signer: KeystoreSigner): String
    }
}
