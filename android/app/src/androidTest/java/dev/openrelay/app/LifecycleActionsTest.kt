package dev.openrelay.app

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.os.Build
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Botones de notificación en un dispositivo real. Cada acción se dispara con
 * su PendingIntent real (`Notification.Action.actionIntent.send()`), no con
 * una llamada directa al servicio.
 *
 * La retirada de tarea real NO está aquí: quitar la tarea mata el proceso de
 * la instrumentación. Eso lo hace un probe externo (scripts/test-lifecycle-actions.sh).
 */
@RunWith(AndroidJUnit4::class)
class LifecycleActionsTest {

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context: Context = instrumentation.targetContext
    private var scenario: ActivityScenario<MainActivity>? = null

    @Before
    fun grantAndShow() {
        StreamService.resetForTests()
        instrumentation.uiAutomation.grantRuntimePermission(
            context.packageName, Manifest.permission.RECORD_AUDIO
        )
        if (Build.VERSION.SDK_INT >= 33) {
            instrumentation.uiAutomation.grantRuntimePermission(
                context.packageName, Manifest.permission.POST_NOTIFICATIONS
            )
        }
        scenario = ActivityScenario.launch(MainActivity::class.java)
        scenario!!.moveToState(Lifecycle.State.RESUMED)
        assertTrue(
            "la Activity tiene que publicar visibilidad antes del FGS",
            waitUntil(3_000) { ActivityPresence.shown.get() },
        )
    }

    @After
    fun stopAndClose() {
        try {
            StreamService.stop(context)
            waitUntil(3_000) { !StreamService.running.value }
        } catch (t: Throwable) {
            // El test ya falló o el servicio no llegó a arrancar.
        }
        scenario?.close()
        scenario = null
        StreamService.resetForTests()
    }

    @Test
    fun notificationButtonsMuteUnmuteAndStopViaRealPendingIntents() {
        StreamService.start(context)
        assertTrue(
            "FGS no arrancó: " + StreamService.errorState.value,
            waitUntil(8_000) { StreamService.running.value },
        )
        assertTrue(waitUntil(3_000) { titles().contains("Mute") })

        action("Mute").actionIntent.send()
        assertTrue("Mute via PendingIntent does not mute", waitUntil(3_000) { StreamService.muted.value })
        assertTrue(
            "after mute the notification shows Unmute: ${titles()}",
            waitUntil(3_000) { titles().contains("Unmute") },
        )

        action("Unmute").actionIntent.send()
        assertTrue("Unmute via PendingIntent does not unmute", waitUntil(3_000) { !StreamService.muted.value })
        assertTrue(waitUntil(3_000) { titles().contains("Mute") })

        action("Stop").actionIntent.send()
        assertTrue("Stop via PendingIntent does not stop the FGS", waitUntil(5_000) { !StreamService.running.value })
        assertTrue("the notification must be removed after Stop", waitUntil(3_000) { activeNotification() == null })
    }

    /** Sin parar: la instrumentación queda viva para que el probe externo quite la tarea. */
    @Test
    fun holdFgsForExternalTaskRemoval() {
        StreamService.start(context)
        assertTrue(
            "FGS no arrancó: " + StreamService.errorState.value,
            waitUntil(8_000) { StreamService.running.value },
        )
        Thread.sleep(120_000)
    }

    private fun titles(): List<String> =
        activeNotification()?.actions?.mapNotNull { it.title?.toString() }.orEmpty()

    private fun action(title: String): Notification.Action {
        val n = activeNotification()
        assertNotNull("sin notificación de FGS", n)
        return n!!.actions.firstOrNull { it.title?.toString() == title }
            ?: error("sin acción '$title': ${titles()}")
    }

    private fun activeNotification(): Notification? {
        val nm = context.getSystemService(NotificationManager::class.java)
        return nm.activeNotifications.firstOrNull { it.packageName == context.packageName }?.notification
    }

    private fun waitUntil(timeoutMs: Long, predicate: () -> Boolean): Boolean {
        val deadline = System.nanoTime() + timeoutMs * 1_000_000L
        while (System.nanoTime() < deadline) {
            if (predicate()) return true
            Thread.sleep(100)
        }
        return predicate()
    }
}
