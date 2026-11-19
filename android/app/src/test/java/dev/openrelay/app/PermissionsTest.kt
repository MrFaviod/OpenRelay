package dev.openrelay.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Permisos sin dispositivo. No pide ni concede nada: fija el contrato que el
 * manifiesto y el servicio tienen que cumplir con target 35.
 * Robolectric no está en el catálogo y Gradle está fuera de este alcance,
 * así que la comprobación contra PackageManager vive en FgsLifecycleTest.
 */
class PermissionsTest {

    @Test
    fun installTimePermissionsAreTheFourNonRuntimeOnes() {
        assertEquals(
            listOf(
                "android.permission.INTERNET",
                "android.permission.ACCESS_NETWORK_STATE",
                "android.permission.FOREGROUND_SERVICE",
                "android.permission.FOREGROUND_SERVICE_MICROPHONE",
            ),
            Permissions.INSTALL_TIME,
        )
    }

    @Test
    fun recordAudioIsTheOnlyForegroundBlocker() {
        assertEquals("android.permission.RECORD_AUDIO", Permissions.RECORD_AUDIO)
        assertFalse(Permissions.INSTALL_TIME.contains(Permissions.RECORD_AUDIO))
        assertFalse(Permissions.INSTALL_TIME.contains(Permissions.POST_NOTIFICATIONS))
    }

    @Test
    fun notificationPermissionNameIsPostNotifications() {
        assertEquals("android.permission.POST_NOTIFICATIONS", Permissions.POST_NOTIFICATIONS)
    }

    @Test
    fun localNetworkPermissionIsNotDeclaredAtTarget35() {
        val joined = (Permissions.INSTALL_TIME + listOf(
            Permissions.RECORD_AUDIO,
            Permissions.POST_NOTIFICATIONS,
        )).joinToString(" ")
        assertFalse(joined.contains("ACCESS_LOCAL_NETWORK"))
    }

    @Test
    fun serviceActionsAreLocalAndExplicit() {
        assertEquals("dev.openrelay.app.action.START", StreamService.ACTION_START)
        assertEquals("dev.openrelay.app.action.STOP", StreamService.ACTION_STOP)
        assertEquals("dev.openrelay.app.action.MUTE", StreamService.ACTION_MUTE)
        assertEquals("dev.openrelay.app.action.UNMUTE", StreamService.ACTION_UNMUTE)
        assertEquals("dev.openrelay.app.action.CONTENT", StreamService.ACTION_CONTENT)
        assertTrue(StreamService.ACTION_START.startsWith("dev.openrelay.app."))
    }

    @Test
    fun flowsStartIdle() {
        StreamService.resetForTests()
        assertFalse(StreamService.running.value)
        assertFalse(StreamService.muted.value)
        assertEquals(0f, StreamService.level.value)
        assertEquals(null, StreamService.errorState.value)
        assertEquals("", StreamService.notificationWarning.value)
        assertFalse(ActivityPresence.shown.get())
    }
}
