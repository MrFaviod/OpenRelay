package dev.arl.audio

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Smoke del retorno: carga libarlcapture.so y ejercita create -> start ->
 * write (silencio) -> stop -> destroy. Depende de que el equipo tenga salida
 * de audio; el resultado queda en logcat con el tag ARLPlayback.
 */
@RunWith(AndroidJUnit4::class)
class PlaybackEngineSmokeTest {

    @Test
    fun playbackSmoke() {
        val engine = PlaybackEngine()
        var rcStart = Int.MIN_VALUE
        var rcWrite = Int.MIN_VALUE
        var rcStop = Int.MIN_VALUE
        var rcDestroy = Int.MIN_VALUE

        try {
            engine.enabled = true
            engine.acknowledgeFeedbackWarning()
            rcStart = engine.start()
            Log.i(TAG, "playbackSmoke start rc=$rcStart")
            assertEquals("start debe devolver OK", 0, rcStart)

            rcWrite = engine.write(ShortArray(FRAME_SAMPLES))
            Log.i(TAG, "playbackSmoke write rc=$rcWrite")
            assertTrue("write debe aceptar muestras (rc=$rcWrite)", rcWrite >= 0)

            rcStop = engine.stop()
            Log.i(TAG, "playbackSmoke stop rc=$rcStop")
        } finally {
            rcDestroy = engine.destroy()
            Log.i(TAG, "playbackSmoke destroy rc=$rcDestroy")
        }

        assertEquals("stop debe devolver OK", 0, rcStop)
        assertEquals("destroy debe devolver OK", 0, rcDestroy)
    }

    private companion object {
        const val TAG = "ARLPlayback"
        const val FRAME_SAMPLES = 240
    }
}
