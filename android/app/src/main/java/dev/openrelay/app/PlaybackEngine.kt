package dev.openrelay.app

/**
 * Política pura del retorno Linux→Android (D-08: fase separada, opt-in y
 * apagada por defecto). Vive separada del JNI para poder probarse en la JVM:
 * sin `enabled` y sin aviso de eco confirmado, no se arranca la salida.
 */
class PlaybackPolicy {
    @Volatile
    var enabled: Boolean = false

    @Volatile
    private var feedbackAcknowledged: Boolean = false

    fun acknowledgeFeedbackWarning() {
        feedbackAcknowledged = true
    }

    fun canStart(): Boolean = enabled && feedbackAcknowledged
}

/**
 * Vida del handle nativo de salida, fuera del JNI para poder probarse en la
 * JVM: la lib sólo existe en el dispositivo. Sin objeto nativo —create falló
 * o destroy ya lo liberó— start/write/stop/destroy no entran al JNI y
 * devuelven [PlaybackEngine.ERROR_NO_HANDLE].
 *
 * Lo escribe y lo lee el hilo dueño de la salida; [value] es volátil porque
 * el chequeo y el destroy pueden verse desde otro hilo del servicio.
 */
class PlaybackHandle {
    @Volatile
    private var value: Long = 0L

    /** Handle devuelto por create. 0 deja el objeto inservible. */
    fun attach(created: Long) {
        value = created
    }

    /** destroy aceptado: el objeto nativo ya no existe. */
    fun release() {
        value = 0L
    }

    /** Handle para la llamada nativa; 0 si no hay objeto nativo. */
    fun live(): Long = value

    /** Código con el que se rechaza la operación; 0 si puede ir al JNI. */
    fun refusal(): Int = if (value == 0L) PlaybackEngine.ERROR_NO_HANDLE else 0
}

/**
 * Salida de retorno Linux→Android: envoltorio fino sobre libopenrelay_capture.so
 * (Oboe output PCM16 48 kHz mono). Flujo: create -> start -> write (bucle) ->
 * stop -> destroy. write devuelve muestras aceptadas (>= 0) o un código de
 * error negativo. Nunca toca el flujo de captura: no comparte stream ni estado.
 *
 * destroy es obligatorio y suelta el objeto nativo. Tras destroy el handle
 * queda en 0 y toda operación devuelve [ERROR_NO_HANDLE] sin tocar el JNI:
 * nadie escribe sobre un stream ya liberado.
 */
class PlaybackEngine {
    private val policy = PlaybackPolicy()
    private val life = PlaybackHandle()

    init {
        life.attach(nativeCreate())
    }

    /** Opt-in del retorno; false por defecto. */
    var enabled: Boolean
        get() = policy.enabled
        set(value) {
            policy.enabled = value
        }

    /** El usuario confirmó el aviso de eco/feedback del retorno. */
    fun acknowledgeFeedbackWarning() = policy.acknowledgeFeedbackWarning()

    /** Sin opt-in o sin aviso confirmado devuelve ERROR_FEEDBACK_NOT_ACKNOWLEDGED. */
    fun start(): Int {
        if (!policy.canStart()) return ERROR_FEEDBACK_NOT_ACKNOWLEDGED
        val refused = life.refusal()
        if (refused != 0) return refused
        return nativeStart(life.live())
    }

    fun write(dst: ShortArray): Int {
        val refused = life.refusal()
        if (refused != 0) return refused
        return nativeWrite(life.live(), dst)
    }

    fun stop(): Int {
        val refused = life.refusal()
        if (refused != 0) return refused
        return nativeStop(life.live())
    }

    /** Libera el objeto nativo; si devuelve 0 el handle queda en 0. */
    fun destroy(): Int {
        val refused = life.refusal()
        if (refused != 0) return refused
        val rc = nativeDestroy(life.live())
        if (rc == NATIVE_OK) life.release()
        return rc
    }

    private external fun nativeCreate(): Long
    private external fun nativeStart(h: Long): Int
    private external fun nativeWrite(h: Long, dst: ShortArray): Int
    private external fun nativeStop(h: Long): Int
    private external fun nativeDestroy(h: Long): Int

    companion object {
        /** start() sin opt-in o sin aviso de eco confirmado. */
        const val ERROR_FEEDBACK_NOT_ACKNOWLEDGED = -8

        /** Operación sobre un handle ya destruido (o create fallido): no hay JNI que tocar. */
        const val ERROR_NO_HANDLE = -9

        private const val NATIVE_OK = 0

        init {
            System.loadLibrary("openrelay_capture")
        }
    }
}
