package dev.openrelay.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

/** El URI del QR se valida antes de tocar Keystore o red. */
class PairingUriTest {

    private val pin = "a".repeat(64)
    private val token = "b".repeat(64)

    @Test
    fun parseaElUriCompleto() {
        val parsed = PairingUriParser.parse(
            "openrelay://pair?v=1&h=192.168.1.20:47001&pin=$pin&t=$token",
        )
        assertEquals("192.168.1.20", parsed.host)
        assertEquals(47001, parsed.port)
        assertEquals(pin, parsed.pinHex)
        assertEquals(token, parsed.tokenHex)
    }

    @Test
    fun aceptaMayusculasYEspacios() {
        val parsed = PairingUriParser.parse(
            "  openrelay://pair?v=1&h=pc.local:47000&pin=${pin.uppercase()}&t=${token.uppercase()}  ",
        )
        assertEquals("pc.local", parsed.host)
        assertEquals(pin, parsed.pinHex)
    }

    @Test
    fun rechazaVersionDistinta() {
        assertThrows(IllegalArgumentException::class.java) {
            PairingUriParser.parse("openrelay://pair?v=2&h=pc:47000&pin=$pin&t=$token")
        }
    }

    @Test
    fun rechazaPinCorto() {
        assertThrows(IllegalArgumentException::class.java) {
            PairingUriParser.parse("openrelay://pair?v=1&h=pc:47000&pin=abc&t=$token")
        }
    }

    @Test
    fun rechazaPuertoFueraDeRango() {
        assertThrows(IllegalArgumentException::class.java) {
            PairingUriParser.parse("openrelay://pair?v=1&h=pc:70000&pin=$pin&t=$token")
        }
    }

    @Test
    fun rechazaOtroEsquema() {
        assertNull(PairingUriParser.parseOrNull("https://example.com/pair?v=1"))
    }

    @Test
    fun parseaHostIpv6() {
        val (host, port) = PairingUriParser.parseHostPort("[fd00::1]:47000")
        assertEquals("fd00::1", host)
        assertEquals(47000, port)
    }
}
