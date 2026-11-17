package dev.arl.audio

import android.Manifest
import android.app.Notification
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TestName
import org.junit.runner.RunWith

/**
 * FGS de micrófono en un dispositivo real. La Activity se deja en RESUMED
 * antes de arrancar el servicio. No se llama a onDestroy para simular la
 * muerte del proceso: eso no mata el proceso. stopWithTask y START_NOT_STICKY
 * se leen del manifiesto y de onStartCommand.
 *
 * El emulador sin audio real no demuestra un micrófono físico: aquí se exige
 * el ciclo nativo (start OK) y la notificación, y se registra el nivel.
 */
@RunWith(AndroidJUnit4::class)
class FgsLifecycleTest {

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context: Context = instrumentation.targetContext
    private var scenario: ActivityScenario<MainActivity>? = null

    @get:Rule
    val testName = TestName()


    @Before
    fun grantAndShow() {
        StreamService.resetForTests()
        val method = testName.getMethodName()
        // Revocar desde este proceso mata la instrumentación en API 34.
        // Cada denegación se prepara con `adb shell pm revoke` y la app parada,
        // y se lanza ese método solo. Aquí no se vuelve a conceder lo revocado.
        if (method != "withoutRecordAudioItDoesNotRunAndSaysWhy") {
            instrumentation.uiAutomation.grantRuntimePermission(
                context.packageName, Manifest.permission.RECORD_AUDIO
            )
        }
        if (Build.VERSION.SDK_INT >= 33 &&
            method != "deniedNotificationsDoNotBlockTheForegroundService"
        ) {
            instrumentation.uiAutomation.grantRuntimePermission(
                context.packageName, Manifest.permission.POST_NOTIFICATIONS
            )
        }
        scenario = ActivityScenario.launch(MainActivity::class.java)
        scenario!!.moveToState(Lifecycle.State.RESUMED)
        var resumed = false
        scenario!!.onActivity { resumed = !it.isFinishing && !it.isDestroyed }
        assertTrue("la Activity tiene que estar viva antes del FGS", resumed)
        assertTrue(
            "la visibilidad la publica el ciclo real de la Activity, no un atajo",
            ActivityPresence.shown.get(),
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
    fun manifestDeclaresMicForegroundServiceAndNoLocalNetwork() {
        val pm = context.packageManager
        @Suppress("DEPRECATION")
        val pkg = pm.getPackageInfo(context.packageName, PackageManager.GET_PERMISSIONS)
        val perms = pkg.requestedPermissions?.toList().orEmpty()
        assertTrue(perms.contains(Manifest.permission.RECORD_AUDIO))
        assertTrue(perms.contains(Manifest.permission.INTERNET))
        assertTrue(perms.contains(Manifest.permission.ACCESS_NETWORK_STATE))
        assertTrue(perms.contains(Manifest.permission.FOREGROUND_SERVICE))
        assertTrue(perms.contains(Manifest.permission.FOREGROUND_SERVICE_MICROPHONE))
        assertTrue(perms.contains(Manifest.permission.POST_NOTIFICATIONS))
        // API 37 añade ACCESS_LOCAL_NETWORK por split-permission a targets <37
        // con INTERNET. PackageInfo no refleja sólo lo declarado por el APK.
        // Se comprueba el manifiesto binario, no se ignora esa condición.
        context.assets.openXmlResourceParser("AndroidManifest.xml").use { parser ->
            val declared = mutableListOf<String>()
            while (parser.next() != org.xmlpull.v1.XmlPullParser.END_DOCUMENT) {
                if (parser.eventType == org.xmlpull.v1.XmlPullParser.START_TAG &&
                    parser.name == "uses-permission"
                ) {
                    parser.getAttributeValue(
                        "http://schemas.android.com/apk/res/android", "name",
                    )?.let { declared.add(it) }
                }
            }
            assertFalse("El APK no debe declarar LAN con target <37: $declared",
                declared.any { it.contains("ACCESS_LOCAL_NETWORK") })
        }
        if (perms.any { it.contains("ACCESS_LOCAL_NETWORK") }) {
            assertTrue("Sólo se espera el permiso implícito desde API37",
                Build.VERSION.SDK_INT >= 37)
            assertTrue("El permiso implícito corresponde a target <37",
                context.applicationInfo.targetSdkVersion < 37)
        }

        @Suppress("DEPRECATION")
        val info = pm.getServiceInfo(
            ComponentName(context, StreamService::class.java),
            PackageManager.GET_META_DATA,
        )
        assertEquals("microphone", declaredForegroundServiceTypeName(info))
        assertFalse(info.exported)
    }

    @Test
    fun visibleActivityStartsForegroundMicWithStopAndMute() {
        assertTrue(Permissions.canStartForegroundMic(context))
        StreamService.start(context)
        assertTrue(
            "running sólo tras native start y startForeground: " + StreamService.errorState.value,
            waitUntil(8_000) { StreamService.running.value },
        )
        val notification = activeNotification()
        assertNotNull("tiene que haber notificación de FGS", notification)
        val texts = notificationText(notification!!)
        assertTrue(texts.any { it.contains("Parar") || it.contains("micrófono") })
        val actions = notification.actions?.map { it.title?.toString() }.orEmpty()
        assertTrue("acción Parar: $actions", actions.any { it == "Parar" })
        assertTrue("acción Silenciar: $actions", actions.any { it == "Silenciar" })

        StreamService.mute(context)
        assertTrue(waitUntil(2_000) { StreamService.muted.value })
        // La notificación se publica en el hilo principal después del comando.
        assertTrue(waitUntil(2_000) {
            val titles = activeNotification()?.actions?.map { it.title?.toString() }.orEmpty()
            titles.any { it == "Activar sonido" }
        })
        val muted = activeNotification()
        val mutedActions = muted?.actions?.map { it.title?.toString() }.orEmpty()
        assertTrue("tras mute la acción pasa a Activar sonido: $mutedActions",
            mutedActions.any { it == "Activar sonido" })

        StreamService.stop(context)
        assertTrue(waitUntil(4_000) { !StreamService.running.value })
    }

    /**
     * Método solo. Antes, con la app detenida:
     * `adb shell pm revoke dev.arl.audio android.permission.RECORD_AUDIO`.
     * Revocarlo desde este proceso mata la instrumentación en API 34.
     * No se sale en silencio: si el permiso sigue concedido, el caso falla.
     */
    @Test
    fun withoutRecordAudioItDoesNotRunAndSaysWhy() {
        assertFalse(
            "RECORD_AUDIO sigue concedido; revocar con la app parada antes de este método",
            Permissions.canStartForegroundMic(context),
        )
        assertEquals(
            context.getString(R.string.perm_record_audio_denied),
            Permissions.denialMessage(context),
        )
        StreamService.start(context)
        Thread.sleep(1_500)
        assertFalse(StreamService.running.value)
        val error = StreamService.errorState.value
        assertNotNull(error)
        assertTrue(error!!.contains("micrófono"))
    }

    @Test
    fun deniedNotificationsDoNotBlockTheForegroundService() {
        val sdk = Build.VERSION.SDK_INT
        // POST_NOTIFICATIONS es runtime desde API 33. En 28–32 no hay
        // denegación que preparar y un return aquí cuenta como PASS vacío.
        org.junit.Assume.assumeTrue(
            "POST_NOTIFICATIONS no es runtime en API $sdk; no aplica, no es PASS",
            sdk >= 33,
        )
        // Método solo, con la app parada antes:
        // `adb shell pm revoke dev.arl.audio android.permission.POST_NOTIFICATIONS`.
        // Revocarlo aquí mata el proceso en API 34. No se sale si sigue concedido.
        assertFalse(
            "POST_NOTIFICATIONS sigue concedido; revocar con la app parada antes de este caso",
            Permissions.hasPostNotifications(context),
        )
        assertTrue(Permissions.canStartForegroundMic(context))
        assertFalse(Permissions.hasPostNotifications(context))
        assertTrue(Permissions.notificationVisibilityWarning(context).isNotEmpty())
        StreamService.start(context)
        assertTrue(
            "POST_NOTIFICATIONS denegado no bloquea el FGS: " + StreamService.errorState.value,
            waitUntil(8_000) { StreamService.running.value },
        )
        assertTrue(
            "el aviso de visibilidad queda para la UI: " + StreamService.notificationWarning.value,
            StreamService.notificationWarning.value.contains("notificaciones"),
        )
        // La notificación se construye igual; el sistema puede no mostrarla.
        val built = StreamService.pending(context, StreamService.ACTION_STOP, 9)
        assertNotNull(built)
    }

    @Test
    fun hiddenActivityDoesNotStartTheMicrophone() {
        scenario!!.moveToState(Lifecycle.State.CREATED)
        assertTrue(
            "CREATED tiene que publicar onStop",
            waitUntil(3_000) { !ActivityPresence.shown.get() },
        )
        StreamService.start(context)
        assertTrue(waitUntil(3_000) { StreamService.errorState.value != null })
        assertFalse(StreamService.running.value)
        val error = StreamService.errorState.value
        assertNotNull(error)
        assertTrue(error!!.contains("visible"))
        // El rechazo entra y sale de primer plano; no deja el servicio vivo.
        assertTrue(waitUntil(3_000) { activeNotification() == null })
    }

    private fun activeNotification(): Notification? {
        val nm = context.getSystemService(android.app.NotificationManager::class.java)
        val mine = nm.activeNotifications.firstOrNull { it.packageName == context.packageName }
        return mine?.notification
    }

    private fun notificationText(notification: Notification): List<String> {
        val extras = notification.extras
        return listOfNotNull(
            extras.getCharSequence(Notification.EXTRA_TITLE)?.toString(),
            extras.getCharSequence(Notification.EXTRA_TEXT)?.toString(),
        )
    }

    /**
     * El tipo declarado en el manifiesto. `ServiceInfo.foregroundServiceType`
     * existe desde API 29 y en API 28 el campo no está, así que leerlo ahí
     * devuelve 0 y el oráculo viejo decía "pre-q" sin mirar el APK.
     * `android:foregroundServiceType` sí se empaqueta (0x01010599); en API 28
     * se lee del manifiesto binario. Desde API 29 se usa el campo, que el
     * sistema rellena con ese mismo atributo. El bit de micrófono es 0x80
     * (`FOREGROUND_SERVICE_TYPE_MICROPHONE`).
     */
    private fun declaredForegroundServiceTypeName(
        info: android.content.pm.ServiceInfo,
    ): String {
        val mic = 0x00000080
        val type = if (Build.VERSION.SDK_INT >= 29) {
            info.foregroundServiceType
        } else {
            foregroundServiceTypeFromManifest()
        }
        return if (type and mic != 0) "microphone" else "other($type)"
    }

    private fun foregroundServiceTypeFromManifest(): Int {
        context.assets.openXmlResourceParser("AndroidManifest.xml").use { parser ->
            val typeAttr = "http://schemas.android.com/apk/res/android:foregroundServiceType"
            while (parser.next() != org.xmlpull.v1.XmlPullParser.END_DOCUMENT) {
                if (parser.eventType != org.xmlpull.v1.XmlPullParser.START_TAG) continue
                if (parser.name != "service") continue
                if (parser.getAttributeValue(
                        "http://schemas.android.com/apk/res/android", "name"
                    ) != "dev.arl.audio.StreamService"
                ) continue
                val raw = parser.getAttributeValue(
                    "http://schemas.android.com/apk/res/android", "foregroundServiceType"
                ) ?: error("StreamService no declara $typeAttr")
                return Integer.decode(raw)
            }
        }
        error("AndroidManifest.xml no declara dev.arl.audio.StreamService")
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
