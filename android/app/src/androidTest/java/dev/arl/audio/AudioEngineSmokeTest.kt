package dev.arl.audio

import android.Manifest
import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.rules.TestRule
import org.junit.runners.model.Statement
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Smoke mínimo de AudioEngine. Separa dos cosas:
 *  1) carga de libarlcapture.so + create/destroy (no toca hardware de audio);
 *  2) captura real start/read/stop/close/destroy (depende del audio del emulador).
 * Los resultados se registran con el tag ARLSmoke para leerlos en logcat.
 */
@RunWith(AndroidJUnit4::class)
class AudioEngineSmokeTest {

    /**
     * Equivalente a GrantPermissionRule (artefacto androidx.test:rules no está en el proyecto):
     * concede RECORD_AUDIO vía UiAutomation antes de cada test.
     */
    @get:Rule
    val permission: TestRule = TestRule { base, _ ->
        object : Statement() {
            override fun evaluate() {
                val inst = InstrumentationRegistry.getInstrumentation()
                inst.uiAutomation.grantRuntimePermission(
                    inst.targetContext.packageName, Manifest.permission.RECORD_AUDIO
                )
                base.evaluate()
            }
        }
    }

    @Test
    fun loadCreateDestroy() {
        // El constructor llama a nativeCreate; si devuelve 0 el objeto no existe y read/start fallarían.
        val engine = AudioEngine()
        // destroy exige estado CLOSED (ABI): un objeto recién creado está en OPEN, así que cerramos antes.
        val rcClose = engine.close()
        Log.i(TAG, "loadCreateDestroy close rc=$rcClose")
        assertEquals("close desde OPEN debe devolver OK", 0, rcClose)
        val rcDestroy = engine.destroy()
        Log.i(TAG, "loadCreateDestroy destroy rc=$rcDestroy")
        assertEquals("destroy tras close debe devolver OK", 0, rcDestroy)
    }

    @Test
    fun captureSmoke() {
        val engine = AudioEngine()
        val buf = ShortArray(ARL_FRAME)
        var total = 0L
        var readErrors = 0
        var rcStart = Int.MIN_VALUE
        var rcStop = Int.MIN_VALUE
        var rcClose = Int.MIN_VALUE
        var rcDestroy = Int.MIN_VALUE
        try {
            rcStart = engine.start()
            Log.i(TAG, "captureSmoke start rc=$rcStart")
            assertEquals("start debe devolver OK", 0, rcStart)

            val t0 = System.currentTimeMillis()
            var reads = 0
            while (System.currentTimeMillis() - t0 < READ_WINDOW_MS) {
                val n = engine.read(buf)
                reads++
                if (n < 0) {
                    readErrors++
                    Log.i(TAG, "captureSmoke read rc=$n")
                } else {
                    total += n
                }
                Thread.sleep(10)
            }
            Log.i(TAG, "captureSmoke reads=$reads samples=$total readErrors=$readErrors")
            assertEquals("no debe haber errores de read", 0, readErrors)
            assertTrue("deben llegar muestras (llegaron $total)", total > 0)
        } finally {
            rcStop = engine.stop()
            Log.i(TAG, "captureSmoke stop rc=$rcStop")
            rcClose = engine.close()
            Log.i(TAG, "captureSmoke close rc=$rcClose")
            rcDestroy = engine.destroy()
            Log.i(TAG, "captureSmoke destroy rc=$rcDestroy")
        }
        assertEquals("close debe devolver OK", 0, rcClose)
        assertEquals("destroy debe devolver OK", 0, rcDestroy)
    }

    private companion object {
        const val TAG = "ARLSmoke"
        const val ARL_FRAME = 240
        const val READ_WINDOW_MS = 1000L
    }
}
