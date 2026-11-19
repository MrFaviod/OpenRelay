package dev.openrelay.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Política de retorno: apagado por defecto y sin aviso de eco no arranca. */
class PlaybackPolicyTest {

    @Test
    fun defaultIsOff() {
        val policy = PlaybackPolicy()
        assertFalse("enabled debe empezar en false", policy.enabled)
        assertFalse("sin opt-in no se arranca", policy.canStart())
    }

    @Test
    fun enabledWithoutAckDoesNotStart() {
        val policy = PlaybackPolicy()
        policy.enabled = true
        assertFalse("sin aviso de eco confirmado no se arranca", policy.canStart())
    }

    @Test
    fun ackWithoutEnabledDoesNotStart() {
        val policy = PlaybackPolicy()
        policy.acknowledgeFeedbackWarning()
        assertFalse("el aviso solo no activa el retorno", policy.canStart())
    }

    @Test
    fun enabledAndAckStarts() {
        val policy = PlaybackPolicy()
        policy.enabled = true
        policy.acknowledgeFeedbackWarning()
        assertTrue("opt-in + aviso confirmado permiten arrancar", policy.canStart())
    }
}

/**
 * Guarda del handle nativo, la misma que usan start/write/stop/destroy. La lib
 * no existe en la JVM, así que se prueba la clase pura: sin handle no hay
 * llamada al JNI y la operación devuelve error negativo.
 */
class PlaybackHandleTest {

    @Test
    fun createdHandleIsUsable() {
        val handle = PlaybackHandle()
        handle.attach(41L)
        assertEquals("con handle vivo la operación puede ir", 0, handle.refusal())
        assertEquals("el handle vivo se pasa tal cual", 41L, handle.live())
    }

    @Test
    fun afterDestroyStartAndWriteAreRefused() {
        val handle = PlaybackHandle()
        handle.attach(41L)

        // destroy aceptado: el objeto nativo ya no existe.
        handle.release()

        assertEquals(
            "start y write tras destroy devuelven error",
            PlaybackEngine.ERROR_NO_HANDLE,
            handle.refusal(),
        )
        assertEquals("tras destroy no queda handle que tocar", 0L, handle.live())
        assertTrue("el error debe ser negativo", PlaybackEngine.ERROR_NO_HANDLE < 0)
    }

    @Test
    fun failedCreateHasNoHandle() {
        val handle = PlaybackHandle()
        handle.attach(0L)
        assertEquals(
            "sin create no se llama al JNI",
            PlaybackEngine.ERROR_NO_HANDLE,
            handle.refusal(),
        )
        assertEquals(0L, handle.live())
    }

    @Test
    fun destroyTwiceStaysRefused() {
        val handle = PlaybackHandle()
        handle.attach(7L)
        handle.release()
        handle.release()
        assertEquals(PlaybackEngine.ERROR_NO_HANDLE, handle.refusal())
        assertEquals(0L, handle.live())
    }
}
