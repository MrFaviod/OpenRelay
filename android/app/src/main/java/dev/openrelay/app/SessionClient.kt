package dev.openrelay.app

/**
 * Cliente de sesión de producto. Habla QUIC de sesión (`openrelay-session/1`)
 * por UDP contra el endpoint aprovisionado y control `openrelay.control/v1`.
 *
 * La clave privada no sale de [KeystoreSigner]. La provisión (endpoint,
 * nombre de servidor, ancla DER, pin SPKI y cadena del cliente) la entrega
 * quien llama: este tipo no trae CA de test, no hace TOFU y no abre un
 * segundo protocolo. Un ACK de `config.set` distingue `requested` de
 * `effective` y no acredita audio físico. No pide ni arranca el micrófono.
 *
 * El handle nativo es un id opaco. [close] es local e idempotente.
 */
class SessionClient private constructor(private var handle: Long) : AutoCloseable {

    fun hello(): Int = nativeHello(handle)

    /** `config.get`. Cadena vacía pide el snapshot. Devuelve el `request_id`. */
    fun configGet(key: String): String = nativeConfigGet(handle, key)

    /**
     * `config.set` de `audio.gain_db` con la revisión que el cliente cree
     * vigente. No aplica el valor en el dispositivo: espera el ACK.
     */
    fun configSetGain(expectedRevision: Long, gainDb: Double): String =
        nativeConfigSetGain(handle, expectedRevision, gainDb)

    /**
     * Fija el perfil de envío de audio: `0` PCM16 (defecto), `1` Opus 5 ms,
     * `2` Opus 2,5 ms, `3` Opus 10 ms. Sin servidor que anuncie
     * `audio.opus-48k-mono` la sesión sigue en PCM16. 0 = aplicado.
     */
    fun setCodec(profile: Int): Int = nativeSetCodec(handle, profile)

    /**
     * Envía un bloque PCM16 del perfil activo: 240 muestras (5 ms) en PCM16,
     * o las del perfil Opus. 0 = enviado; el motivo queda en [lastError].
     */
    fun sendAudio(pcm: ShortArray): Int = nativeSendAudio(handle, pcm)

    /**
     * Anuncia (o retira) el retorno `audio.return-pcm16-48k-mono` en las
     * `capabilities` de la sesión. Apagado por defecto: sin esta llamada la
     * feature no viaja y el daemon no emite ni un datagrama de retorno.
     * 0 = aplicado; el motivo queda en [lastError].
     */
    fun setReturnAudio(enabled: Boolean): Int = nativeSetReturnAudio(handle, enabled)

    /**
     * Saca un frame de retorno ya decodificado (240 muestras PCM16 48 kHz
     * mono) y lo copia en [dst]. Devuelve las muestras copiadas, `0` si no
     * hay nada y negativo si la sesión no sirve o [dst] es corto.
     */
    fun readReturn(dst: ShortArray): Int = nativeReadReturn(handle, dst)

    /** Siguiente evento, o cadena vacía. Ver [ControlEvent]. */
    fun poll(): String = nativePoll(handle)

    /** Bloquea hasta autorización TLS o fallo. 0 = autorizado. */
    fun waitAuthorized(): Int = nativeWaitAuthorized(handle)

    /** Bloquea hasta el `ack` o `error` de ese `request_id`, o timeout. */
    fun waitControl(requestId: String): String = nativeWaitControl(handle, requestId)

    fun lastError(): String = nativeLastError()

    override fun close() {
        val id = handle
        if (id == 0L) return
        handle = 0L
        nativeClose(id)
    }

    companion object {
        init {
            System.loadLibrary("openrelay_bridge")
        }

        /**
         * Abre la sesión. `chain` es la cadena del cliente, hoja primero.
         * Devuelve null si la provisión o el socket fallan; el motivo está
         * en [lastOpenError].
         */
        fun open(
            signer: KeystoreSigner,
            endpoint: String,
            serverName: String,
            caDer: ByteArray,
            pinHex: String,
            chain: Array<ByteArray>,
        ): SessionClient? {
            val id = nativeOpen(signer, endpoint, serverName, caDer, pinHex, chain)
            return if (id == 0L) null else SessionClient(id)
        }

        fun lastOpenError(): String = nativeLastError()

        @JvmStatic
        private external fun nativeOpen(
            signer: KeystoreSigner,
            endpoint: String,
            serverName: String,
            caDer: ByteArray,
            pinHex: String,
            chain: Array<ByteArray>,
        ): Long

        @JvmStatic
        private external fun nativeHello(handle: Long): Int

        @JvmStatic
        private external fun nativeConfigGet(handle: Long, key: String): String

        @JvmStatic
        private external fun nativeConfigSetGain(
            handle: Long,
            expectedRevision: Long,
            gainDb: Double,
        ): String

        @JvmStatic
        private external fun nativeSetCodec(handle: Long, profile: Int): Int

        @JvmStatic
        private external fun nativeSendAudio(handle: Long, pcm: ShortArray): Int

        @JvmStatic
        private external fun nativeSetReturnAudio(handle: Long, enabled: Boolean): Int

        @JvmStatic
        private external fun nativeReadReturn(handle: Long, dst: ShortArray): Int

        @JvmStatic
        private external fun nativePoll(handle: Long): String

        @JvmStatic
        private external fun nativeWaitAuthorized(handle: Long): Int

        @JvmStatic
        private external fun nativeWaitControl(handle: Long, requestId: String): String

        @JvmStatic
        private external fun nativeClose(handle: Long): Int

        @JvmStatic
        private external fun nativeLastError(): String
    }
}

/**
 * Línea de evento del puente. `requested` y `effective` son JSON distintos:
 * un ACK no afirma que el valor pedido sea el que el dispositivo aplica,
 * y ninguno de los dos afirma que haya audio.
 */
data class ControlEvent(
    val kind: String,
    val requestId: String = "",
    val revision: Long = -1,
    val applied: Boolean = false,
    val requestedJson: String = "null",
    val effectiveJson: String = "null",
    val code: String = "",
    /** Revisión vigente que reporta un conflicto. `-1` si el error no la trae. */
    val currentRevision: Long = -1,
) {
    companion object {
        fun parse(line: String): ControlEvent? {
            if (line.isEmpty() || line.startsWith("ERR ")) return null
            val parts = line.split('\t')
            return when (parts[0]) {
                "authorized", "rejected", "closed", "io" -> ControlEvent(kind = parts[0])
                "ack" -> ControlEvent(
                    kind = "ack",
                    requestId = parts.getOrElse(1) { "" },
                    revision = parts.getOrElse(2) { "-1" }.toLongOrNull() ?: -1,
                    applied = parts.getOrElse(3) { "false" } == "true",
                    requestedJson = parts.getOrElse(4) { "null" },
                    effectiveJson = parts.getOrElse(5) { "null" },
                )
                "error" -> ControlEvent(
                    kind = "error",
                    requestId = parts.getOrElse(1) { "" },
                    code = parts.getOrElse(2) { "" },
                    currentRevision = parts.getOrElse(3) { "-1" }.toLongOrNull() ?: -1,
                )
                else -> null
            }
        }
    }
}
