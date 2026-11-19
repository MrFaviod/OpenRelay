package dev.openrelay.app

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat

/**
 * Permisos del servicio de micrófono. No arranca el servicio ni concede nada:
 * sólo dice qué falta y cómo pedirlo. Una orden remota no pasa por aquí.
 *
 * Con target 35, ACCESS_LOCAL_NETWORK (API 37) no se declara ni se solicita.
 */
object Permissions {

    const val RECORD_AUDIO: String = Manifest.permission.RECORD_AUDIO

    /** Runtime desde API 33. En API 28–32 las notificaciones no piden este permiso. */
    const val POST_NOTIFICATIONS: String = Manifest.permission.POST_NOTIFICATIONS

    /** Instalados, no runtime. Van en el manifiesto; no se piden al usuario. */
    val INSTALL_TIME: List<String> = listOf(
        Manifest.permission.INTERNET,
        Manifest.permission.ACCESS_NETWORK_STATE,
        Manifest.permission.FOREGROUND_SERVICE,
        Manifest.permission.FOREGROUND_SERVICE_MICROPHONE,
    )

    fun needsNotificationPermission(): Boolean = Build.VERSION.SDK_INT >= 33

    fun hasRecordAudio(context: Context): Boolean = granted(context, RECORD_AUDIO)

    /**
     * API 28–32: siempre true (no existe el runtime). API 33+: true sólo si el
     * usuario lo concedió. Denegarlo no impide el FGS de micrófono.
     */
    fun hasPostNotifications(context: Context): Boolean {
        if (!needsNotificationPermission()) return true
        return granted(context, POST_NOTIFICATIONS)
    }

    /**
     * Lo único que bloquea el FGS. POST_NOTIFICATIONS no entra aquí.
     */
    fun missingForForegroundMic(context: Context): List<String> {
        return if (hasRecordAudio(context)) emptyList() else listOf(RECORD_AUDIO)
    }

    fun canStartForegroundMic(context: Context): Boolean =
        missingForForegroundMic(context).isEmpty()

    /**
     * Qué pedirle al usuario ahora. RECORD_AUDIO siempre que falte.
     * POST_NOTIFICATIONS sólo en API 33+ y sólo si falta; pedirlo junto no
     * convierte su denegación en un bloqueo del micrófono.
     */
    fun runtimeRequests(context: Context): List<String> {
        val out = ArrayList<String>(2)
        if (!hasRecordAudio(context)) out.add(RECORD_AUDIO)
        if (needsNotificationPermission() && !hasPostNotifications(context)) {
            out.add(POST_NOTIFICATIONS)
        }
        return out
    }

    /**
     * Texto accionable cuando el micrófono está denegado. Vacío si está concedido.
     * La denegación de notificaciones no usa este mensaje: el FGS sigue siendo legal.
     */
    fun denialMessage(context: Context): String {
        if (hasRecordAudio(context)) return ""
        return context.getString(R.string.perm_record_audio_denied)
    }

    /**
     * Aviso de visibilidad, no un bloqueo. Vacío en API 28–32 o si está concedido.
     */
    fun notificationVisibilityWarning(context: Context): String {
        if (hasPostNotifications(context)) return ""
        return context.getString(R.string.perm_notifications_hidden)
    }

    /**
     * El FGS de micrófono (while-in-use) sólo puede nacer con una Activity
     * visible: no finished, no destroyed y con ventana. Un test que llama
     * onDestroy a mano no cuenta como muerte de proceso.
     */
    fun isActivityVisible(activity: Activity): Boolean {
        if (activity.isFinishing) return false
        if (activity.isDestroyed) return false
        return activity.hasWindowFocus() || activity.window?.decorView?.isShown == true
    }

    private fun granted(context: Context, permission: String): Boolean =
        ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED
}
