package dev.openrelay.app

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

/**
 * F4: el cliente de producto rechaza una provisión mala sin abrir sesión,
 * parsea un ACK distinguiendo requested de effective, y, con un fixture
 * externo, abre una sesión real sobre UDP con la clave de AndroidKeyStore.
 *
 * El roundtrip necesita un host con session_fixture (sólo test). El Android
 * publica el SPKI público en files/spki.hex; el orquestador del host escribe
 * files/provision (4 líneas: endpoint, CA DER hex, pin hex, hoja DER hex).
 * Sin ese archivo el roundtrip se marca skipped: ausente no es PASS.
 */
@RunWith(AndroidJUnit4::class)
class SessionClientTest {

    @Test
    fun badProvisionDoesNotOpen() {
        val client = SessionClient.open(
            KeystoreSigner(),
            "not-an-endpoint",
            "localhost",
            byteArrayOf(0x30),
            "zz",
            arrayOf(byteArrayOf(0x30)),
        )
        assertNull(client)
        assertTrue(SessionClient.lastOpenError().isNotEmpty())
    }

    @Test
    fun ackLineKeepsRequestedApartFromEffective() {
        val line = "ack\tc-17\t8\ttrue\t{\"key\":\"audio.gain_db\",\"value\":3.0}\t{\"key\":\"audio.gain_db\",\"value\":2.5}"
        val event = ControlEvent.parse(line)
        assertNotNull(event)
        assertEquals("ack", event!!.kind)
        assertEquals("c-17", event.requestId)
        assertEquals(8L, event.revision)
        assertTrue(event.applied)
        assertTrue(event.requestedJson.contains("3.0"))
        assertTrue(event.effectiveJson.contains("2.5"))
        assertTrue(event.requestedJson != event.effectiveJson)
    }

    /**
     * Rechazo de provisión inválida (certificado vacío). No abre sesión y por
     * tanto no ejercita [SessionClient.close]: un null aquí no es PASS de cierre.
     */
    @Test
    fun emptyCertificateIsRejectedBeforeOpen() {
        val client = SessionClient.open(
            KeystoreSigner(),
            "127.0.0.1:9",
            "localhost",
            ByteArray(32),
            "ab".repeat(32),
            arrayOf(ByteArray(32)),
        )
        assertNull(client)
        assertTrue(SessionClient.lastOpenError().isNotEmpty())
    }

    /**
     * Sesión real contra el fixture externo: autorización TLS con la clave del
     * Keystore, hello, config.get, config.set con ACK (requested != effective),
     * conflicto de revisión y cierre idempotente.
     */
    @Test
    fun keystoreSessionRoundtripAgainstExternalFixture() {
        val dir = InstrumentationRegistry.getInstrumentation().targetContext.filesDir
        val signer = KeystoreSigner()
        val spkiTmp = File(dir, "spki.hex.tmp")
        spkiTmp.writeText(hex(signer.publicKeyDer()))
        spkiTmp.renameTo(File(dir, "spki.hex"))

        val provision = File(dir, "provision")
        val deadline = System.currentTimeMillis() + 150_000
        while (!provision.exists() && System.currentTimeMillis() < deadline) {
            Thread.sleep(200)
        }
        assumeTrue("sin fixture externo: provision ausente", provision.exists())

        val lines = provision.readLines()
        assertEquals("provision incompleta", 4, lines.size)
        val endpoint = lines[0]
        val caHex = lines[1]
        val pinHex = lines[2]
        val leafHex = lines[3]

        val client = SessionClient.open(
            signer,
            endpoint,
            "localhost",
            unhex(caHex),
            pinHex,
            arrayOf(unhex(leafHex)),
        )
        assertNotNull("open falló: " + SessionClient.lastOpenError(), client)
        val session = client!!
        try {
            assertEquals("handshake no autorizado: " + session.lastError(), 0, session.waitAuthorized())
            assertEquals(0, session.hello())

            val getId = session.configGet("audio.gain_db")
            val snapshot = ControlEvent.parse(session.waitControl(getId))
            assertNotNull("sin ack de config.get", snapshot)
            assertEquals("ack", snapshot!!.kind)
            assertTrue("config.get sin revisión", snapshot.revision >= 0)

            val setId = session.configSetGain(snapshot.revision, 3.0)
            val ack = ControlEvent.parse(session.waitControl(setId))
            assertNotNull("sin ack de config.set", ack)
            assertEquals("ack", ack!!.kind)
            assertEquals(setId, ack.requestId)
            assertTrue("ack sin applied", ack.applied)
            assertEquals(snapshot.revision + 1, ack.revision)
            assertTrue("requested no es 3.0: " + ack.requestedJson, ack.requestedJson.contains("3.0"))
            assertTrue("effective no es 2.5: " + ack.effectiveJson, ack.effectiveJson.contains("2.5"))
            assertTrue(ack.requestedJson != ack.effectiveJson)

            val staleId = session.configSetGain(snapshot.revision, 1.0)
            val conflict = ControlEvent.parse(session.waitControl(staleId))
            assertNotNull("sin respuesta al conflicto", conflict)
            assertEquals("error", conflict!!.kind)
            assertEquals("conflict", conflict.code)
            assertEquals(ack.revision, conflict.currentRevision)
        } finally {
            session.close()
        }
        session.close()
    }

    private fun hex(bytes: ByteArray): String = bytes.joinToString("") { "%02x".format(it) }

    private fun unhex(text: String): ByteArray {
        val clean = text.trim()
        return ByteArray(clean.length / 2) { i ->
            clean.substring(i * 2, i * 2 + 2).toInt(16).toByte()
        }
    }
}
