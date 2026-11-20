package dev.openrelay.app

/**
 * Captura nativa: envoltorio fino sobre libopenrelay_capture.so (Oboe + core C).
 * Flujo: create -> start -> read (en bucle) -> stop -> close -> destroy.
 * read devuelve muestras copiadas (>= 0) o un código de error negativo.
 * Sin permiso RECORD_AUDIO concedido, start fallará; el permiso lo pide la capa de UI.
 */
class AudioEngine {
    private var handle: Long = nativeCreate()

    fun start(): Int = nativeStart(handle)

    fun read(dst: ShortArray): Int = nativeRead(handle, dst)

    fun stop(): Int = nativeStop(handle)

    fun close(): Int = nativeClose(handle)

    /** Libera el objeto nativo; si devuelve error distinto de 0 el handle sigue vivo. */
    fun destroy(): Int {
        val rc = nativeDestroy(handle)
        if (rc == 0) handle = 0L
        return rc
    }

    private external fun nativeCreate(): Long
    private external fun nativeStart(h: Long): Int
    private external fun nativeRead(h: Long, dst: ShortArray): Int
    private external fun nativeStop(h: Long): Int
    private external fun nativeClose(h: Long): Int
    private external fun nativeDestroy(h: Long): Int

    companion object {
        init {
            System.loadLibrary("openrelay_capture")
        }
    }
}
