package dev.openrelay.app.ui

import dev.openrelay.app.PairingState

/**
 * Modelo puro de la pantalla de inicio. Sin Hilt, sin ViewModel y sin red.
 *
 * [connection] sigue en [Connection.Unavailable]: el transporte lo describe
 * [connectionText], que el servicio publica ("Enviando a <host>" o
 * "Sin conexión: <motivo>") y que la UI no inventa. El emparejamiento es un
 * hecho local ([pairingHost]) más el estado del enrolamiento en curso
 * ([pairingState]); ninguno de los dos implica que haya audio saliendo.
 *
 * El integrador mapea el servicio a [capture], [error] y [notificationWarning].
 * No infiera "transmitiendo" de un micrófono abierto. Un aviso de
 * notificaciones no es un error: denegarlo no para la captura.
 */
data class HomeUiState(
    val capture: Capture = Capture.Idle,
    val connection: Connection = Connection.Unavailable,
    /** Texto del enlace que publica el servicio. Vacío = sin enlace. */
    val connectionText: String = "",
    /** Host del PC emparejado, o null si no hay provisión guardada. */
    val pairingHost: String? = null,
    /** Estado del último intento de enrolamiento. */
    val pairingState: PairingState = PairingState.Idle,
    val error: UiError? = null,
    /**
     * Texto de [StreamService.notificationWarning]. Vacío si no hay aviso.
     * No es [error]: la captura sigue siendo legal. No se persiste: lo
     * republica el servicio.
     */
    val notificationWarning: String = "",
    /**
     * true si esta sesión todavía puede mostrar el pedido de POST_NOTIFICATIONS.
     * false tras una denegación: el aviso abre los ajustes. No se persiste.
     */
    val notificationRequestable: Boolean = false,
    /** 0..1 medido por el servicio. Ignorado si [levelAvailable] es false. */
    val level: Float = 0f,
    val levelAvailable: Boolean = false,
    val advancedExpanded: Boolean = false,
    /** Ganancia pedida en dB, −12..+12. Preferencia local; no es efecto aplicado. */
    val gainRequestedDb: Int = 0,
    val gainEffectiveDb: Int? = null,
    /**
     * Mute que publica el servicio. Con captura en marcha el servicio drena
     * las muestras y publica nivel 0. Sin captura es sólo lo pedido: no hay
     * muestras que silenciar ni nivel que bajar.
     */
    val muted: Boolean = false,
    /**
     * true mientras el hilo de retorno está enganchado. Lo publica el servicio:
     * la UI no lo enciende por su cuenta.
     */
    val returnOn: Boolean = false,
    /** true con sesión autorizada: sin ella el control de retorno no sirve. */
    val returnAvailable: Boolean = false,
    val versionName: String = "",
)

enum class Capture {
    /** El servicio no está capturando. */
    Idle,

    /** Micrófono abierto en el dispositivo. No implica enlace con un PC. */
    Local,
}

enum class Connection {
    /** No hay transporte de producto. La UI debe decirlo en texto. */
    Unavailable,
}

/**
 * Error accionable. [action] nombra el botón; el callback lo resuelve la Activity.
 * null en [action] = sólo texto, sin botón que finja un arreglo.
 */
data class UiError(
    val kind: ErrorKind,
    val action: ErrorAction?,
)

enum class ErrorKind {
    MicPermissionDenied,
    CaptureFailed,
}

enum class ErrorAction {
    GrantPermission,
    OpenSettings,
    Retry,
}

/** Tope de lo que el usuario puede pedir. No es la ganancia que el audio aplica. */
const val GAIN_MIN_DB: Int = -12
const val GAIN_MAX_DB: Int = 12

fun clampGain(db: Int): Int = db.coerceIn(GAIN_MIN_DB, GAIN_MAX_DB)

/**
 * Reduce un gesto de la UI. No abre el micrófono, no arranca un servicio y no
 * toca la red: sólo describe la intención para que la Activity la despache.
 */
fun reduce(state: HomeUiState, event: HomeEvent): HomeUiState = when (event) {
    HomeEvent.ToggleAdvanced -> state.copy(advancedExpanded = !state.advancedExpanded)
    is HomeEvent.SetGainRequested -> state.copy(gainRequestedDb = clampGain(event.db))
    HomeEvent.ToggleMute -> state.copy(muted = !state.muted)
}

sealed interface HomeEvent {
    data object ToggleAdvanced : HomeEvent
    data class SetGainRequested(val db: Int) : HomeEvent
    data object ToggleMute : HomeEvent
}
