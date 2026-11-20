package dev.openrelay.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.roundToInt
import kotlin.math.sqrt

/**
 * FGS de micrófono. Un solo hilo posee el AudioEngine (create/start/read/stop/
 * close/destroy). Nadie más llama al nativo. read devuelve 0 cuando el anillo
 * está vacío: el hilo espera, no gira.
 *
 * running pasa a true sólo si startForeground fue aceptado y native start
 * devolvió 0, en ese orden: el micrófono no se abre antes. START_NOT_STICKY:
 * el sistema no reengancha el micrófono. No hay receptor de arranque ni camino
 * remoto. Un intent sin acción no es un arranque.
 *
 * API de integración (la UI llama sólo a esto):
 *  - Valores de sólo lectura: running, muted, level (RMS 0..1), errorState,
 *    notificationWarning. No es kotlinx StateFlow: es [AppState], un valor
 *    con collect, porque el catálogo no trae coroutines.
 *  - start(context): pide el FGS. Falla sin RECORD_AUDIO o sin Activity visible.
 *  - stop(context), mute(context), unmute(context): acciones locales. Si no hay
 *    captura, el servicio se detiene y no queda vivo.
 *  - noteActivityVisible / noteActivityHidden: la UI los llama desde onStart y
 *    onStop de la Activity. Es el camino de visibilidad; no hay atajo de test.
 * La notificación repite Parar y Silenciar con intents explícitos inmutables.
 */
class StreamService : Service() {

    private val stopRequested = stopRequestedFlag
    @Volatile private var worker: Thread? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val action = intent?.action
        return try {
            when (action) {
                ACTION_STOP -> {
                    if (worker?.isAlive == true) requestStop() else finishOrphan()
                    START_NOT_STICKY
                }
                ACTION_MUTE -> {
                    if (worker?.isAlive != true) {
                        finishOrphan()
                    } else {
                        _muted.set(true)
                        refreshNotification()
                    }
                    START_NOT_STICKY
                }
                ACTION_UNMUTE -> {
                    if (worker?.isAlive != true) {
                        finishOrphan()
                    } else {
                        _muted.set(false)
                        refreshNotification()
                    }
                    START_NOT_STICKY
                }
                ACTION_START -> beginCapture()
                else -> {
                    // null o acción desconocida: no se abre el micrófono.
                    finishOrphan()
                    START_NOT_STICKY
                }
            }
        } catch (t: Throwable) {
            Log.e(TAG, "onStartCommand failed action=$action", t)
            fail(t.javaClass.simpleName + ": " + (t.message ?: "command"))
            finishOrphan()
            START_NOT_STICKY
        }
    }

    /** Arranque pedido. Un stop que llegó antes gana: no se abre el micrófono. */
    private fun beginCapture(): Int {
        if (stopRequested.get()) {
            finishOrphan()
            return START_NOT_STICKY
        }
        if (!Permissions.canStartForegroundMic(this)) {
            refuse(getString(R.string.stream_error_mic_denied))
            return START_NOT_STICKY
        }
        if (!isAppVisible()) {
            refuse(getString(R.string.stream_error_not_visible))
            return START_NOT_STICKY
        }
        val warning = Permissions.notificationVisibilityWarning(this)
        _notificationWarning.set(warning)
        startWorker()
        return START_NOT_STICKY
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        requestStop()
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        // Primero la bandera (el bucle de retorno la mira) y después el
        // retorno: su stop espera al hilo antes de tocar la salida.
        requestStop()
        detachReturnPlayback()
        joinWorker()
        super.onDestroy()
    }

    private fun startWorker() {
        synchronized(gate) {
            if (worker?.isAlive == true) return
            if (stopRequested.get()) return
            val thread = Thread({ runCapture() }, WORKER_NAME)
            worker = thread
            thread.start()
        }
    }

    private fun runCapture() {
        val engine: AudioEngine = try {
            AudioEngine()
        } catch (t: Throwable) {
            Log.e(TAG, "AudioEngine() failed", t)
            fail(getString(R.string.stream_error_engine))
            stopSelf()
            return
        }
        var foreground = false
        var started = false
        try {
            // Primero el primer plano. Si el sistema lo rechaza, el micrófono
            // no llega a abrirse.
            if (!promoteForeground()) return
            foreground = true
            val rcStart = engine.start()
            if (rcStart != NATIVE_OK) {
                Log.e(TAG, "native start rc=$rcStart")
                fail(getString(R.string.stream_error_engine) + " (start $rcStart)")
                return
            }
            started = true
            _errorState.set(null)
            _running.set(true)
            readLoop(engine)
        } catch (t: Throwable) {
            Log.e(TAG, "capture failed", t)
            fail(t.javaClass.simpleName + ": " + (t.message ?: "error"))
        } finally {
            shutdown(engine, foreground, started)
        }
    }

    /** true si el proceso quedó en primer plano. Si no, el micrófono no se abre. */
    private fun promoteForeground(): Boolean {
        return try {
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                buildNotification(),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
            )
            true
        } catch (t: Throwable) {
            Log.e(TAG, "startForeground rejected", t)
            fail(t.javaClass.simpleName + ": " + (t.message ?: "foreground"))
            false
        }
    }

    private fun readLoop(engine: AudioEngine) {
        val buf = ShortArray(FRAME_SAMPLES)
        val pairing = PairingStore(this).load()
        var link: SessionClient? = null
        var backoffMs = BACKOFF_MIN_MS
        var nextAttempt = 0L
        if (pairing != null) {
            _link.set(getString(R.string.link_connecting, pairing.host))
            link = openLink(pairing)
            if (link == null) {
                backoffMs = (backoffMs * 2).coerceAtMost(BACKOFF_MAX_MS)
                nextAttempt = System.currentTimeMillis() + backoffMs
            }
        } else {
            _link.set("")
        }
        try {
            while (!stopRequested.get()) {
                if (pairing != null && link == null && System.currentTimeMillis() >= nextAttempt) {
                    _link.set(getString(R.string.link_connecting, pairing.host))
                    link = openLink(pairing)
                    if (link != null) {
                        backoffMs = BACKOFF_MIN_MS
                    } else {
                        backoffMs = (backoffMs * 2).coerceAtMost(BACKOFF_MAX_MS)
                        nextAttempt = System.currentTimeMillis() + backoffMs
                        // Si el PC ya no acepta este teléfono (por ejemplo tras
                        // `devices remove`), no dejar la UI en "Conectando…".
                        _link.set(
                            getString(
                                R.string.link_offline,
                                SessionClient.lastOpenError().ifEmpty {
                                    getString(R.string.link_unknown_error)
                                },
                            ),
                        )
                    }
                }
                if (_muted.value) {
                    // Mute efectivo: se drena y no se publica nivel. El nativo sigue abierto.
                    val dropped = engine.read(buf)
                    if (dropped < 0) {
                        reportReadError(dropped)
                        return
                    }
                    _level.set(0f)
                    sleepQuiet(POLL_MS)
                    continue
                }
                val n = engine.read(buf)
                when {
                    n < 0 -> {
                        reportReadError(n)
                        return
                    }
                    n == 0 -> sleepQuiet(POLL_MS)
                    else -> {
                        _level.set(rms(buf, n))
                        val active = link ?: continue
                        val frame = if (n == buf.size) buf else buf.copyOf(n)
                        if (active.sendAudio(frame) != NATIVE_OK) {
                            // Un fallo de envío no corta el micrófono local:
                            // se cierra el enlace y se reintenta con backoff.
                            val detail = SessionClient.lastOpenError().ifEmpty {
                                getString(R.string.link_unknown_error)
                            }
                            active.close()
                            link = null
                            activeLink = null
                            _sessionActive.set(false)
                            _returnActive.set(false)
                            _gainEffective.set(null)
                            _link.set(getString(R.string.link_offline, detail))
                            nextAttempt = System.currentTimeMillis() + backoffMs
                        } else {
                            // El envío puede aceptarse en el buffer local aunque
                            // el daemon ya haya cerrado o rechazado la sesión.
                            // Los eventos nativos son la señal fiable.
                            val event = ControlEvent.parse(active.poll())
                            when {
                                event == null -> {}
                                event.kind == "ack" -> {
                                    if (event.revision >= 0) linkRevision = event.revision
                                    if (event.requestId == pendingGain) pendingGain = null
                                    if (event.requestedJson.contains("audio.gain_db")) {
                                        _gainEffective.set(effectiveGainDb(event.effectiveJson))
                                    }
                                }
                                event.kind == "error" -> {
                                    if (event.currentRevision >= 0) {
                                        linkRevision = event.currentRevision
                                    }
                                    if (event.code == "conflict" &&
                                        event.requestId == pendingGain &&
                                        event.currentRevision >= 0
                                    ) {
                                        // El PC cambió la revisión antes de mover
                                        // el slider: se reenvía el mismo valor.
                                        pendingGain = active.configSetGain(
                                            event.currentRevision,
                                            pendingGainDb.toDouble(),
                                        )
                                    }
                                }
                                event.kind == "closed" ||
                                    event.kind == "rejected" ||
                                    event.kind == "io" -> {
                                    val detail = when (event.kind) {
                                        "rejected" -> getString(R.string.link_refused)
                                        "closed" -> getString(R.string.link_closed)
                                        else -> getString(R.string.link_unknown_error)
                                    }
                                    active.close()
                                    link = null
                                    activeLink = null
                                    _sessionActive.set(false)
                                    _returnActive.set(false)
                                    _gainEffective.set(null)
                                    _link.set(getString(R.string.link_offline, detail))
                                    nextAttempt = System.currentTimeMillis() + backoffMs
                                    backoffMs = (backoffMs * 2).coerceAtMost(BACKOFF_MAX_MS)
                                }
                            }
                        }
                    }
                }
            }
        } finally {
            link?.close()
            activeLink = null
            _sessionActive.set(false)
            _returnActive.set(false)
            _gainEffective.set(null)
            _link.set("")
        }
    }

    /**
     * Abre la sesión con la provisión del emparejamiento y espera la
     * autorización TLS. Null si el PC no responde o rechaza.
     */
    private fun openLink(pairing: Pairing): SessionClient? {
        return try {
            val signer = KeystoreSigner()
            val endpoint = if (pairing.host.contains(':')) {
                "[${pairing.host}]:${pairing.port}"
            } else {
                "${pairing.host}:${pairing.port}"
            }
            val client = SessionClient.open(
                signer,
                endpoint,
                pairing.serverName,
                pairing.caDer,
                pairing.serverPinHex,
                arrayOf(pairing.leafDer),
            ) ?: return null
            if (client.waitAuthorized() != NATIVE_OK) {
                client.close()
                return null
            }
            // La sesión queda publicada para que la UI pueda pedir ganancia; la
            // revisión arranca en 0, el default del catálogo.
            activeLink = client
            _sessionActive.set(true)
            linkRevision = 0L
            _gainEffective.set(null)
            _link.set(getString(R.string.link_sending, pairing.host))
            client
        } catch (t: Throwable) {
            Log.w(TAG, "session open failed: " + t.javaClass.simpleName)
            null
        }
    }

    private fun reportReadError(rc: Int) {
        Log.e(TAG, "native read rc=$rc")
        fail(getString(R.string.stream_error_engine) + " (read $rc)")
    }

    /**
     * Retorno Linux→Android sobre la sesión activa. Sólo arranca si
     * [PlaybackEngine.start] acepta (opt-in y aviso de eco confirmados):
     * `start()` aplica la misma política que `PlaybackPolicy.canStart()` y
     * devuelve [PlaybackEngine.ERROR_FEEDBACK_NOT_ACKNOWLEDGED] si falta
     * cualquiera de las dos. Sin eso no hay hilo ni salida.
     *
     * El toggle y el aviso visual siguen pendientes: hoy nadie llama a
     * [attachReturnPlayback], así que el servicio se comporta igual que antes.
     */
    internal class ReturnPump(
        private val session: SessionClient,
        private val playback: PlaybackEngine,
    ) {
        @Volatile private var running = false
        @Volatile private var closed = false
        private var thread: Thread? = null

        /** true si hay hilo de retorno. Sin política favorable devuelve false. */
        fun start(): Boolean {
            if (closed) return false
            if (thread?.isAlive == true) return true
            if (playback.start() != NATIVE_OK) {
                // Sin hilo no hay quien escriba: el handle se suelta aquí.
                playback.destroy()
                return false
            }
            if (session.setReturnAudio(true) != NATIVE_OK) {
                playback.stop()
                playback.destroy()
                return false
            }
            running = true
            val pump = Thread({ loop() }, RETURN_THREAD_NAME)
            thread = pump
            pump.start()
            return true
        }

        /** Lee del puente y escribe en la salida; nunca gira en vacío. */
        private fun loop() {
            val buf = ShortArray(RETURN_FRAME_SAMPLES)
            while (running && !stopRequestedFlag.get()) {
                val n = session.readReturn(buf)
                when {
                    // Sesión cerrada o handle inservible: se sale sin ruido.
                    n < 0 -> return
                    n == 0 -> pause()
                    n == RETURN_FRAME_SAMPLES -> {
                        val written = playback.write(buf)
                        if (written < 0) return
                    }
                }
            }
        }

        /** Espera del sondeo. No usa métodos de la instancia del servicio. */
        private fun pause() {
            try {
                Thread.sleep(RETURN_POLL_MS)
            } catch (interrupted: InterruptedException) {
                Thread.currentThread().interrupt()
                running = false
            }
        }

        /**
         * Para el hilo y la salida, en ese orden. Idempotente. running=false
         * más interrupt: el hilo sale del bucle tras el write en curso. Si el
         * join vence, el hilo sigue vivo y el handle NO se destruye (se deja
         * log): liberarlo dejaría al hilo escribiendo en un stream muerto.
         */
        fun stop(): Boolean {
            if (closed) return true
            closed = true
            running = false
            val pump = thread
            thread = null
            if (pump != null) {
                pump.interrupt()
                if (!joinUntilExit(pump)) {
                    Log.e(TAG, "return pump alive after ${RETURN_JOIN_BUDGET_MS}ms: playback handle kept")
                    return false
                }
            }
            playback.stop()
            val rc = playback.destroy()
            if (rc != NATIVE_OK) Log.w(TAG, "playback destroy rc=$rc")
            return true
        }

        /** Join con tope; true sólo si el hilo murió. */
        private fun joinUntilExit(pump: Thread): Boolean {
            val deadline = System.nanoTime() + RETURN_JOIN_BUDGET_NS
            while (pump.isAlive && System.nanoTime() < deadline) {
                try {
                    pump.join(POLL_MS)
                } catch (interrupted: InterruptedException) {
                    Thread.currentThread().interrupt()
                    break
                }
            }
            return !pump.isAlive
        }
    }

    /**
     * stop -> close -> destroy, en ese orden y sólo desde este hilo.
     * Cada retorno que no cierra se publica en errorState, no sólo en logcat.
     * BUSY (-5) se reintenta con tope. destroy sólo si close cerró.
     */
    private fun shutdown(engine: AudioEngine, foreground: Boolean, started: Boolean) {
        _running.set(false)
        _level.set(0f)
        val problems = ArrayList<String>(3)
        try {
            if (started) {
                val rcStop = callBounded("stop") { engine.stop() }
                if (rcStop != NATIVE_OK && rcStop != ERR_STATE && rcStop != ERR_CLOSED) {
                    problems.add("stop $rcStop")
                }
            }
            val rcClose = callBounded("close") { engine.close() }
            val closed = rcClose == NATIVE_OK || rcClose == ERR_CLOSED
            if (!closed) {
                problems.add("close $rcClose")
            } else {
                val rcDestroy = callBounded("destroy") { engine.destroy() }
                if (rcDestroy != NATIVE_OK) problems.add("destroy $rcDestroy")
            }
        } catch (t: Throwable) {
            Log.e(TAG, "shutdown failed", t)
            problems.add(t.javaClass.simpleName)
        } finally {
            if (problems.isNotEmpty()) {
                val message = getString(R.string.stream_error_cleanup) + " " + problems.joinToString(", ")
                Log.e(TAG, message)
                _errorState.set(message)
            }
            if (foreground) {
                ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            }
            stopSelf()
        }
    }

    /** Reintenta sólo ERR_BUSY, con techo de intentos y de tiempo. */
    private fun callBounded(name: String, call: () -> Int): Int {
        val deadline = System.nanoTime() + SHUTDOWN_BUDGET_NS
        var rc = ERR_BUSY
        var attempt = 0
        while (attempt < SHUTDOWN_ATTEMPTS && System.nanoTime() < deadline) {
            attempt += 1
            rc = call()
            if (rc != ERR_BUSY) return rc
            Log.w(TAG, "native $name busy attempt=$attempt")
            sleepQuiet(POLL_MS)
        }
        return rc
    }

    private fun requestStop() {
        stopRequested.set(true)
    }

    /** stop/mute/unmute sin captura no dejan el servicio vivo. */
    private fun finishOrphan() {
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun joinWorker() {
        val thread = worker ?: return
        val deadline = System.nanoTime() + JOIN_BUDGET_NS
        while (thread.isAlive && System.nanoTime() < deadline) {
            try {
                thread.join(POLL_MS)
            } catch (interrupted: InterruptedException) {
                Thread.currentThread().interrupt()
                break
            }
        }
        if (thread.isAlive) {
            val message = getString(R.string.stream_error_cleanup) + " worker"
            Log.e(TAG, message)
            _errorState.set(message)
        }
    }

    private fun fail(message: String) {
        _running.set(false)
        _level.set(0f)
        _errorState.set(message)
        Log.e(TAG, message)
    }

    /**
     * Rechazo de un startForegroundService: hay que entrar en primer plano y
     * salir enseguida. running no se publica. El micrófono no se abre.
     */
    private fun refuse(message: String) {
        fail(message)
        try {
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                buildNotification(),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
            )
        } catch (t: Throwable) {
            Log.e(TAG, "refuse startForeground failed", t)
        }
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun isAppVisible(): Boolean = ActivityPresence.visible()

    private fun refreshNotification() {
        if (!_running.value) return
        val nm = getSystemService(NotificationManager::class.java) ?: return
        nm.notify(NOTIFICATION_ID, buildNotification())
    }

    private fun buildNotification(): Notification {
        ensureChannel()
        val text = when {
            _errorState.value != null -> getString(R.string.stream_notification_error)
            _muted.value -> getString(R.string.stream_notification_muted)
            else -> getString(R.string.stream_notification_capturing)
        }
        val muteAction = if (_muted.value) ACTION_UNMUTE else ACTION_MUTE
        val muteLabel = if (_muted.value) R.string.stream_action_unmute else R.string.stream_action_mute
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_mic)
            .setContentTitle(getString(R.string.stream_notification_title))
            .setContentText(text)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setContentIntent(pending(this, ACTION_CONTENT, REQ_CONTENT))
            .addAction(0, getString(R.string.stream_action_stop), pending(this, ACTION_STOP, REQ_STOP))
            .addAction(0, getString(muteLabel), pending(this, muteAction, REQ_MUTE))
            .build()
    }

    private fun ensureChannel() {
        if (Build.VERSION.SDK_INT < 26) return
        val nm = getSystemService(NotificationManager::class.java) ?: return
        if (nm.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.stream_channel_name),
            NotificationManager.IMPORTANCE_LOW,
        )
        channel.description = getString(R.string.stream_channel_description)
        channel.setSound(null, null)
        nm.createNotificationChannel(channel)
    }

    private fun rms(buf: ShortArray, n: Int): Float {
        var acc = 0.0
        val count = n.coerceAtMost(buf.size)
        var i = 0
        while (i < count) {
            val s = buf[i] / 32768.0
            acc += s * s
            i += 1
        }
        if (count == 0) return 0f
        return sqrt(acc / count).toFloat().coerceIn(0f, 1f)
    }

    private fun sleepQuiet(ms: Long) {
        try {
            Thread.sleep(ms)
        } catch (interrupted: InterruptedException) {
            Thread.currentThread().interrupt()
            stopRequested.set(true)
        }
    }

    companion object {
        private const val TAG = "OpenRelayStream"
        private const val WORKER_NAME = "openrelay-capture"
        private const val CHANNEL_ID = "openrelay_mic"
        private const val NOTIFICATION_ID = 19
        private const val FRAME_SAMPLES = 240
        private const val RETURN_FRAME_SAMPLES = 240
        private const val RETURN_THREAD_NAME = "openrelay-return"
        private const val RETURN_POLL_MS = 20L
        private const val POLL_MS = 20L
        private const val BACKOFF_MIN_MS = 500L
        private const val BACKOFF_MAX_MS = 4_000L
        private const val NATIVE_OK = 0
        private const val ERR_STATE = -2
        private const val ERR_BUSY = -5
        private const val ERR_CLOSED = -6
        private const val SHUTDOWN_ATTEMPTS = 8
        private const val SHUTDOWN_BUDGET_NS = 2_000_000_000L
        private const val JOIN_BUDGET_NS = 3_000_000_000L
        private const val RETURN_JOIN_BUDGET_MS = 3_000L
        private const val RETURN_JOIN_BUDGET_NS = RETURN_JOIN_BUDGET_MS * 1_000_000L
        private const val REQ_CONTENT = 1
        private const val REQ_STOP = 2
        private const val REQ_MUTE = 3

        const val ACTION_START = "dev.openrelay.app.action.START"
        const val ACTION_STOP = "dev.openrelay.app.action.STOP"
        const val ACTION_MUTE = "dev.openrelay.app.action.MUTE"
        const val ACTION_UNMUTE = "dev.openrelay.app.action.UNMUTE"
        const val ACTION_CONTENT = "dev.openrelay.app.action.CONTENT"

        /**
         * Lo ven el companion (start/stop) y la instancia (onStartCommand).
         * Sin esto un stop que llega antes que el worker pierde contra el
         * arranque, que volvía a bajar la bandera.
         */
        internal val stopRequestedFlag = AtomicBoolean(false)

        private val gate = Any()

        /** Retorno activo. Null mientras nadie enganche una sesión. */
        @Volatile private var returnPump: ReturnPump? = null
        private val _running = AppState(false)
        private val _muted = AppState(false)
        private val _level = AppState(0f)
        private val _errorState = AppState<String?>(null)
        private val _notificationWarning = AppState("")
        private val _link = AppState("")
        private val _sessionActive = AppState(false)
        private val _returnActive = AppState(false)

        /** true sólo tras startForeground aceptado y native start OK. */
        val running: AppState<Boolean> = _running

        /** Mute local. No libera el micrófono. */
        val muted: AppState<Boolean> = _muted

        /** RMS de la última lectura, 0..1. 0 mientras está silenciado o parado. */
        val level: AppState<Float> = _level

        /** null si no hay error. Un error de cierre también llega aquí. */
        val errorState: AppState<String?> = _errorState

        /**
         * Aviso de visibilidad cuando POST_NOTIFICATIONS está denegado.
         * No bloquea el FGS. Vacío si las notificaciones se pueden mostrar.
         * La UI lo pinta; no es un error de captura.
         */
        val notificationWarning: AppState<String> = _notificationWarning

        /**
         * Estado del enlace con el PC: vacío si no hay emparejamiento,
         * "Conectando…", "Enviando a <host>" o "Sin conexión: <motivo>".
         */
        val link: AppState<String> = _link

        /**
         * true mientras hay una sesión autorizada con el PC. El control de
         * retorno sólo se habilita con una sesión viva: sin ella no hay a dónde
         * devolver el audio.
         */
        val sessionActive: AppState<Boolean> = _sessionActive

        /**
         * true mientras el hilo de retorno está enganchado y la salida arrancó.
         * No es el opt-in: es el estado observado del bombeo.
         */
        val returnActive: AppState<Boolean> = _returnActive

        /** Sesión viva, publicada para que la UI pida ganancia. Null sin enlace. */
        @Volatile private var activeLink: SessionClient? = null

        /** Revisión que el último ACK o conflicto reportó. 0 al abrir. */
        @Volatile private var linkRevision = 0L

        /**
         * Set de gain en vuelo: `request_id` y valor pedido. Un conflicto de
         * revisión se reenvía una vez con la revisión que reporta el PC.
         */
        @Volatile private var pendingGain: String? = null
        @Volatile private var pendingGainDb = 0

        private val _gainEffective = AppState<Int?>(null)

        /**
         * Ganancia efectiva que confirmó el PC. null mientras no haya ACK: la
         * UI dice "unavailable" y no inventa un valor aplicado.
         */
        val gainEffective: AppState<Int?> = _gainEffective

        fun start(context: Context) {
            stopRequestedFlag.set(false)
            _errorState.set(null)
            _notificationWarning.set(Permissions.notificationVisibilityWarning(context))
            dispatch(context, ACTION_START, foreground = true)
        }

        fun stop(context: Context) {
            stopRequestedFlag.set(true)
            dispatch(context, ACTION_STOP, foreground = false)
        }

        fun mute(context: Context) {
            dispatch(context, ACTION_MUTE, foreground = false)
        }

        fun unmute(context: Context) {
            dispatch(context, ACTION_UNMUTE, foreground = false)
        }

        /**
         * Pide `audio.gain_db` a la sesión abierta con la revisión que el
         * cliente cree vigente. false si no hay sesión: el pedido queda local y
         * la UI no debe afirmar que el PC lo recibió.
         */
        fun requestGain(db: Int): Boolean {
            val link = activeLink ?: return false
            val id = link.configSetGain(linkRevision, db.toDouble())
            if (id.isEmpty() || id.startsWith("ERR")) {
                Log.w(TAG, "gain set refused: $id")
                return false
            }
            pendingGain = id
            pendingGainDb = db
            return true
        }

        /**
         * Un comando que el sistema rechaza no tumba la Activity: el fallo
         * queda en [errorState]. start va por startForegroundService; el resto
         * por startService, que en un FGS ya vivo no abre otro primer plano.
         */
        private fun dispatch(context: Context, action: String, foreground: Boolean) {
            val intent = Intent(context, StreamService::class.java).setAction(action)
            try {
                if (foreground) context.startForegroundService(intent) else context.startService(intent)
            } catch (t: Throwable) {
                Log.e(TAG, "command rejected action=$action", t)
                _errorState.set(t.javaClass.simpleName + ": " + (t.message ?: "command"))
            }
        }

        /**
         * La Activity lo llama en onStart. Es el único camino que abre el FGS:
         * no hay bandera de test ni provider.
         */
        fun noteActivityVisible() {
            ActivityPresence.noteActivityVisible()
        }

        /** La Activity lo llama en onStop. Sin esto el FGS no arranca. */
        fun noteActivityHidden() {
            ActivityPresence.noteActivityHidden()
        }

        /**
         * Engancha el retorno a la sesión y la salida indicadas. Si
         * [PlaybackEngine.start] no acepta (sin opt-in o sin aviso de eco
         * confirmado), no se crea hilo ni se toca la salida: el servicio
         * queda exactamente como estaba. Es el punto de entrada que usará el
         * toggle, todavía pendiente.
         */
        fun attachReturnPlayback(session: SessionClient, playback: PlaybackEngine): Boolean {
            synchronized(gate) {
                // Si el hilo anterior no murió, su handle sigue vivo: no se
                // arranca otro encima y el motor nuevo no se queda colgado.
                if (returnPump?.stop() == false) {
                    playback.destroy()
                    return false
                }
                val pump = ReturnPump(session, playback)
                returnPump = pump
                if (pump.start()) {
                    _returnActive.set(true)
                    return true
                }
                returnPump = null
                _returnActive.set(false)
                return false
            }
        }

        /**
         * Engancha el retorno a la sesión viva. false sin sesión o si el
         * bombeo no arranca: la UI no debe decir que el retorno está activo.
         */
        fun attachReturnToActiveSession(playback: PlaybackEngine): Boolean {
            val session = activeLink ?: return false
            return attachReturnPlayback(session, playback)
        }

        /**
         * Apaga el retorno y avisa al PC con `setReturnAudio(false)`. Sin
         * sesión sólo para el bombeo: no hay a quién avisar.
         */
        fun detachReturnAndNotify(): Boolean {
            detachReturnPlayback()
            val session = activeLink ?: return false
            return session.setReturnAudio(false) == NATIVE_OK
        }

        /** Suelta el retorno: para el hilo y la salida. Idempotente. */
        fun detachReturnPlayback() {
            synchronized(gate) {
                returnPump?.stop()
                returnPump = null
                _returnActive.set(false)
            }
        }

        /** PendingIntent explícito e inmutable. content abre MainActivity; el resto, el servicio. */
        fun pending(context: Context, action: String, requestCode: Int): PendingIntent {
            val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            val intent = if (action == ACTION_CONTENT) {
                Intent(context, MainActivity::class.java).setAction(action)
            } else {
                Intent(context, StreamService::class.java).setAction(action)
            }
            return if (action == ACTION_CONTENT) {
                PendingIntent.getActivity(context, requestCode, intent, flags)
            } else {
                PendingIntent.getService(context, requestCode, intent, flags)
            }
        }

        internal fun resetForTests() {
            stopRequestedFlag.set(false)
            _running.set(false)
            _muted.set(false)
            _level.set(0f)
            _errorState.set(null)
            _notificationWarning.set("")
            _link.set("")
            _sessionActive.set(false)
            _returnActive.set(false)
            ActivityPresence.shown.set(false)
        }
    }
}

/**
 * Presencia de la Activity. La pone la UI con [StreamService.noteActivityVisible]
 * y [StreamService.noteActivityHidden]. El servicio no adivina la ventana.
 */
internal object ActivityPresence {
    val shown = AtomicBoolean(false)
    fun visible(): Boolean = shown.get()

    /** La Activity lo llama en onStart. Sin esto el FGS no arranca. */
    fun noteActivityVisible() {
        shown.set(true)
    }

    /** La Activity lo llama en onStop. */
    fun noteActivityHidden() {
        shown.set(false)
    }
}

/** dB enteros del JSON de `effective` de un ACK de `audio.gain_db`. */
internal fun effectiveGainDb(json: String): Int? = json.trim().toDoubleOrNull()?.roundToInt()

/**
 * Valor observable sin coroutines. No es kotlinx.coroutines.flow.StateFlow:
 * no tiene conflate, replay ni contexto. value se lee; set lo escribe el
 * dueño. collect entrega el valor actual y los siguientes.
 */
class AppState<T>(initial: T) {
    @Volatile private var current: T = initial
    private val collectors = CopyOnWriteArraySet<(T) -> Unit>()

    val value: T get() = current

    fun collect(collector: (T) -> Unit) {
        collectors.add(collector)
        collector(current)
    }

    fun remove(collector: (T) -> Unit) {
        collectors.remove(collector)
    }

    internal fun set(next: T) {
        current = next
        for (collector in collectors) {
            try {
                collector(next)
            } catch (t: Throwable) {
                Log.e("OpenRelayStream", "collector failed", t)
            }
        }
    }
}
