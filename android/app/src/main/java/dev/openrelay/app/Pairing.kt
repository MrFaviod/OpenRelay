package dev.openrelay.app

import android.content.Context
import android.util.Base64
import java.net.URI

/**
 * URI del QR que muestra el PC:
 * `openrelay://pair?v=1&h=HOST:PORT&pin=<64 hex>&t=<64 hex>`.
 *
 * Sólo se acepta la versión 1 y el pin/token en hexadecimal de 32 bytes. El
 * parseo no toca la red ni el Keystore: es texto.
 */
data class PairingUri(
    val host: String,
    val port: Int,
    val pinHex: String,
    val tokenHex: String,
)

/**
 * Nombre DNS del certificado persistente del daemon (`Server.der` generado por
 * la provisión local incluye `openrelay`, `localhost`, `127.0.0.1` y `::1`).
 * El TLS de sesión se valida contra él, no contra la IP del QR.
 */
private const val SESSION_TLS_NAME = "openrelay"

object PairingUriParser {

    /** Lanza [IllegalArgumentException] con el motivo si el URI no sirve. */
    fun parse(raw: String): PairingUri {
        val uri = try {
            URI(raw.trim())
        } catch (error: Exception) {
            throw IllegalArgumentException("URI de emparejamiento ilegible", error)
        }
        val scheme = uri.scheme?.lowercase()
        if (scheme != "openrelay" || uri.host?.lowercase() != "pair") {
            throw IllegalArgumentException("no es un URI openrelay://pair")
        }
        val params = queryParams(uri.rawQuery)
        if (params["v"] != "1") {
            throw IllegalArgumentException("versión de emparejamiento no soportada")
        }
        val pin = params["pin"]?.lowercase()
        if (pin == null || !HEX_64.matches(pin)) {
            throw IllegalArgumentException("pin del QR no es 64 hex")
        }
        val token = params["t"]?.lowercase()
        if (token == null || !HEX_64.matches(token)) {
            throw IllegalArgumentException("token del QR no es 64 hex")
        }
        val (host, port) = parseHostPort(params["h"])
        return PairingUri(host = host, port = port, pinHex = pin, tokenHex = token)
    }

    fun parseOrNull(raw: String): PairingUri? = try {
        parse(raw)
    } catch (_: IllegalArgumentException) {
        null
    }

    /** `host:puerto`, con IPv6 entre corchetes. */
    fun parseHostPort(value: String?): Pair<String, Int> {
        if (value.isNullOrBlank()) {
            throw IllegalArgumentException("falta h=host:puerto")
        }
        val (host, portText) = if (value.startsWith("[")) {
            val end = value.indexOf("]:")
            if (end < 0) throw IllegalArgumentException("h no es host:puerto")
            value.substring(1, end) to value.substring(end + 2)
        } else {
            val colon = value.lastIndexOf(':')
            if (colon <= 0) throw IllegalArgumentException("h no es host:puerto")
            value.substring(0, colon) to value.substring(colon + 1)
        }
        val port = portText.toIntOrNull()
        if (host.isEmpty() || port == null || port !in 1..65535) {
            throw IllegalArgumentException("h no es host:puerto")
        }
        return host to port
    }

    private fun queryParams(rawQuery: String?): Map<String, String> {
        if (rawQuery.isNullOrEmpty()) return emptyMap()
        return rawQuery.split('&')
            .mapNotNull { part ->
                val index = part.indexOf('=')
                if (index <= 0) null else part.substring(0, index) to part.substring(index + 1)
            }
            .toMap()
    }

    private val HEX_64 = Regex("^[0-9a-f]{64}$")
}

/**
 * Provisión persistida tras el enrolamiento. `caDer` y `leafDer` son DER
 * binarios tal como llegan del puente; se guardan en base64 en preferencias
 * privadas de la app. `serverName` es el nombre DNS que el certificado
 * persistente del daemon acredita en el TLS de sesión (`openrelay`); el host
 * del QR es sólo el endpoint al que se conecta.
 */
class Pairing(
    val host: String,
    val port: Int,
    val caDer: ByteArray,
    val leafDer: ByteArray,
    val serverName: String,
    /**
     * Pin SPKI del servidor de sesión. El QR trae el pin del endpoint de
     * enrolamiento y el daemon usa la misma identidad persistente
     * (`quic.identity`) para sesión, así que el pin es el mismo.
     */
    val serverPinHex: String,
)

class PairingStore(context: Context) {

    private val prefs = context.applicationContext
        .getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun load(): Pairing? {
        val host = prefs.getString(KEY_HOST, null) ?: return null
        val port = prefs.getInt(KEY_PORT, 0)
        val ca = prefs.getString(KEY_CA, null) ?: return null
        val leaf = prefs.getString(KEY_LEAF, null) ?: return null
        if (port !in 1..65535) return null
        return try {
            Pairing(
                host = host,
                port = port,
                caDer = Base64.decode(ca, Base64.NO_WRAP),
                leafDer = Base64.decode(leaf, Base64.NO_WRAP),
                serverName = prefs.getString(KEY_NAME, null) ?: SESSION_TLS_NAME,
                serverPinHex = prefs.getString(KEY_PIN, "") ?: "",
            )
        } catch (_: IllegalArgumentException) {
            null
        }
    }

    fun save(pairing: Pairing) {
        prefs.edit()
            .putString(KEY_HOST, pairing.host)
            .putInt(KEY_PORT, pairing.port)
            .putString(KEY_CA, Base64.encodeToString(pairing.caDer, Base64.NO_WRAP))
            .putString(KEY_LEAF, Base64.encodeToString(pairing.leafDer, Base64.NO_WRAP))
            .putString(KEY_NAME, pairing.serverName)
            .putString(KEY_PIN, pairing.serverPinHex)
            .apply()
    }

    fun clear() {
        prefs.edit().clear().apply()
    }

    private companion object {
        const val PREFS = "openrelay_pairing"
        const val KEY_HOST = "host"
        const val KEY_PORT = "port"
        const val KEY_CA = "ca_der"
        const val KEY_LEAF = "leaf_der"
        const val KEY_NAME = "server_name"
        const val KEY_PIN = "server_pin"
    }
}

sealed interface PairingState {
    data object Idle : PairingState
    data class Waiting(val host: String) : PairingState
    data class Paired(val host: String) : PairingState
    data class Failed(val message: String) : PairingState
}

/**
 * Enrolamiento en un hilo aparte: el puente bloquea hasta que el usuario
 * aprueba en el PC. La UI observa [state]; nadie más toca el Keystore.
 */
object PairingFlow {

    private val _state = AppState<PairingState>(PairingState.Idle)
    val state: AppState<PairingState> = _state

    @Volatile private var worker: Thread? = null

    /** Arranca el enrolamiento. Si ya hay uno en curso, no lanza otro. */
    fun start(context: Context, uri: PairingUri) {
        synchronized(this) {
            if (worker?.isAlive == true) return
            _state.set(PairingState.Waiting(uri.host))
            val app = context.applicationContext
            val thread = Thread({ run(app, uri) }, "openrelay-pairing")
            worker = thread
            thread.start()
        }
    }

    fun forget(context: Context) {
        PairingStore(context).clear()
        _state.set(PairingState.Idle)
    }

    private fun run(context: Context, uri: PairingUri) {
        try {
            val signer = KeystoreSigner()
            val result = nativeEnroll(
                uri.host,
                uri.port,
                uri.pinHex,
                uri.tokenHex,
                signer.publicKeyDer(),
            )
            if (result == null) {
                _state.set(PairingState.Failed(SessionClient.lastOpenError()))
                return
            }
            if (result.size < 3 || result[0].isEmpty() || result[1].isEmpty() || result[2].size < 2) {
                _state.set(PairingState.Failed("respuesta de enrolamiento incompleta"))
                return
            }
            val port = ((result[2][0].toInt() and 0xff) shl 8) or (result[2][1].toInt() and 0xff)
            if (port == 0) {
                _state.set(PairingState.Failed("el PC no anunció puerto de sesión"))
                return
            }
            PairingStore(context).save(
                Pairing(
                    host = uri.host,
                    port = port,
                    caDer = result[1],
                    leafDer = result[0],
                    serverName = SESSION_TLS_NAME,
                    serverPinHex = uri.pinHex,
                ),
            )
            _state.set(PairingState.Paired(uri.host))
        } catch (error: Throwable) {
            _state.set(
                PairingState.Failed(
                    error.javaClass.simpleName + ": " + (error.message ?: "enrolamiento"),
                ),
            )
        }
    }

    /**
     * Devuelve `[hoja DER, CA de sesión DER, puerto BE16]` o null; el motivo
     * queda en [SessionClient.lastOpenError]. Bloquea hasta la aprobación.
     */
    private external fun nativeEnroll(
        host: String,
        port: Int,
        pinHex: String,
        tokenHex: String,
        spkiDer: ByteArray,
    ): Array<ByteArray>?

    init {
        System.loadLibrary("openrelay_bridge")
    }
}
