package dev.arl.audio

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import dev.arl.audio.ui.ArlTheme
import dev.arl.audio.ui.Capture
import dev.arl.audio.ui.Connection
import dev.arl.audio.ui.ErrorAction
import dev.arl.audio.ui.ErrorKind
import dev.arl.audio.ui.HomeActions
import dev.arl.audio.ui.HomeEvent
import dev.arl.audio.ui.HomeScreen
import dev.arl.audio.ui.HomeUiState
import dev.arl.audio.ui.HomeUiStateSaver
import dev.arl.audio.ui.UiError
import dev.arl.audio.ui.reduce

/**
 * Presenta [HomeScreen] y traduce el servicio de captura a ese modelo.
 *
 * Cableado a la API real de [StreamService]: [StreamService.running],
 * [StreamService.muted], [StreamService.level], [StreamService.errorState] y
 * [StreamService.notificationWarning], más [Permissions.hasRecordAudio] y
 * [Permissions.RECORD_AUDIO]. `running` es captura local, no una conexión:
 * [Connection] queda en Unavailable.
 *
 * La visibilidad del FGS la publica esta Activity: [onStart] llama
 * [StreamService.noteActivityVisible] y [onStop] llama
 * [StreamService.noteActivityHidden]. No hay provider ni atajo de test.
 *
 * Un aviso de notificaciones no es un error de captura. Si el sistema todavía
 * puede mostrar el pedido, el botón pide [Permissions.POST_NOTIFICATIONS];
 * si ya lo denegó, abre los ajustes de notificaciones. Denegarlo no para el FGS.
 *
 * Con la captura en marcha el silencio del servicio drena las muestras y
 * publica nivel 0. Sin captura, el silencio es sólo lo pedido: no hay nivel
 * que bajar. La ganancia del avanzado es lo pedido. El servicio no aplica
 * ganancia, así que la efectiva permanece null. La versión sale de
 * [PackageManager], no de BuildConfig, que este módulo no genera.
 */
class MainActivity : ComponentActivity() {
    private val resumeTick = mutableIntStateOf(0)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            val tick = resumeTick.intValue
            ArlTheme { HomeRoute(tick) }
        }
    }

    override fun onStart() {
        super.onStart()
        StreamService.noteActivityVisible()
    }

    override fun onStop() {
        StreamService.noteActivityHidden()
        super.onStop()
    }

    override fun onResume() {
        super.onResume()
        resumeTick.intValue++
    }
}

@Composable
private fun HomeRoute(resumeTick: Int) {
    val context = LocalContext.current
    var local by rememberSaveable(stateSaver = HomeUiStateSaver) {
        mutableStateOf(HomeUiState(connection = Connection.Unavailable))
    }
    var notificationRequestable by androidx.compose.runtime.remember {
        mutableStateOf(canAskNotifications())
    }
    val running by collect(StreamService.running)
    val muted by collect(StreamService.muted)
    val level by collect(StreamService.level)
    val errorText by collect(StreamService.errorState)
    val notificationWarning by collect(StreamService.notificationWarning)
    val versionName = androidx.compose.runtime.remember { installedVersionName(context) }

    // Al volver de Ajustes el permiso puede haber cambiado sin un callback.
    LaunchedEffect(resumeTick, running) {
        local = dropStaleMicDenial(local, Permissions.hasRecordAudio(context), running)
    }

    val permissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            local = dropStaleMicDenial(local, micGranted = true, running = running)
            StreamService.start(context)
        } else {
            local = local.copy(
                error = UiError(ErrorKind.MicPermissionDenied, ErrorAction.OpenSettings),
            )
        }
    }

    val notificationLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        // Denegar no para la captura. El aviso se queda y el botón pasa a ajustes.
        notificationRequestable = false
    }

    val shown = present(
        local = local,
        running = running,
        muted = muted,
        level = level,
        errorText = errorText,
        micGranted = Permissions.hasRecordAudio(context),
        notificationWarning = notificationWarning,
        notificationRequestable = notificationRequestable,
    ).copy(versionName = versionName)

    Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
        HomeScreen(
            state = shown,
            actions = HomeActions(
                onMicClick = { onMic(context, running) { permissionLauncher.launch(it) } },
                onMuteClick = {
                    if (muted) StreamService.unmute(context) else StreamService.mute(context)
                },
                onErrorAction = { action ->
                    when (action) {
                        ErrorAction.GrantPermission ->
                            permissionLauncher.launch(Permissions.RECORD_AUDIO)
                        ErrorAction.OpenSettings -> openAppSettings(context)
                        ErrorAction.Retry -> {
                            local = local.copy(error = null)
                            onMic(context, running = false) { permissionLauncher.launch(it) }
                        }
                    }
                },
                onGainChange = { db -> local = reduce(local, HomeEvent.SetGainRequested(db)) },
                onToggleAdvanced = { local = reduce(local, HomeEvent.ToggleAdvanced) },
                onNotificationAction = {
                    if (notificationRequestable && canAskNotifications()) {
                        notificationLauncher.launch(Permissions.POST_NOTIFICATIONS)
                    } else {
                        openNotificationSettings(context)
                    }
                },
            ),
            modifier = Modifier.padding(innerPadding),
        )
    }
}

private fun onMic(context: Context, running: Boolean, ask: (String) -> Unit) {
    when {
        running -> StreamService.stop(context)
        Permissions.hasRecordAudio(context) -> StreamService.start(context)
        else -> ask(Permissions.RECORD_AUDIO)
    }
}

/**
 * Un rechazo de micrófono deja de mostrarse en cuanto el permiso es válido o
 * la captura está en marcha. Si no, quedaría pegado después de concederlo en
 * Ajustes. Un error del servicio sigue teniendo prioridad.
 */
internal fun dropStaleMicDenial(
    local: HomeUiState,
    micGranted: Boolean,
    running: Boolean,
): HomeUiState {
    val stuck = local.error?.kind == ErrorKind.MicPermissionDenied && (micGranted || running)
    return if (stuck) local.copy(error = null) else local
}

/**
 * El servicio manda en captura, mute, nivel y error de motor. Conexión y
 * ganancia efectiva no se inventan.
 */
internal fun present(
    local: HomeUiState,
    running: Boolean,
    muted: Boolean,
    level: Float,
    errorText: String?,
    micGranted: Boolean,
    notificationWarning: String = "",
    notificationRequestable: Boolean = false,
): HomeUiState {
    val fresh = dropStaleMicDenial(local, micGranted, running)
    val serviceError = errorText?.let { UiError(ErrorKind.CaptureFailed, ErrorAction.Retry) }
    return fresh.copy(
        capture = if (running) Capture.Local else Capture.Idle,
        connection = Connection.Unavailable,
        muted = muted,
        level = if (muted) 0f else level.coerceIn(0f, 1f),
        levelAvailable = running,
        error = serviceError ?: fresh.error,
        gainEffectiveDb = null,
        notificationWarning = notificationWarning,
        notificationRequestable = notificationRequestable && notificationWarning.isNotEmpty(),
    )
}

/** versionName del paquete instalado. Vacío si el sistema no lo tiene. */
internal fun installedVersionName(context: Context): String = try {
    val info = if (Build.VERSION.SDK_INT >= 33) {
        context.packageManager.getPackageInfo(
            context.packageName,
            PackageManager.PackageInfoFlags.of(0),
        )
    } else {
        @Suppress("DEPRECATION")
        context.packageManager.getPackageInfo(context.packageName, 0)
    }
    info.versionName.orEmpty()
} catch (_: PackageManager.NameNotFoundException) {
    ""
}

private fun openAppSettings(context: Context) {
    val intent = Intent(
        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
        Uri.fromParts("package", context.packageName, null),
    ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    runCatching { context.startActivity(intent) }
}

/** Ajustes de notificaciones de esta app. Si el sistema no los tiene, los de la app. */
private fun openNotificationSettings(context: Context) {
    val intent = if (Build.VERSION.SDK_INT >= 26) {
        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName)
    } else {
        Intent(
            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            Uri.fromParts("package", context.packageName, null),
        )
    }.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    runCatching { context.startActivity(intent) }
        .onFailure { openAppSettings(context) }
}

/**
 * true sólo en API 33+, donde POST_NOTIFICATIONS es un permiso de runtime.
 * En API 28–32 no hay diálogo que mostrar.
 */
private fun canAskNotifications(): Boolean = Build.VERSION.SDK_INT >= 33

/** Observa un [AppState] del servicio. No es un StateFlow de coroutines. */
@Composable
private fun <T> collect(flow: AppState<T>): State<T> {
    val state = androidx.compose.runtime.remember(flow) { mutableStateOf(flow.value) }
    DisposableEffect(flow) {
        val collector: (T) -> Unit = { state.value = it }
        flow.collect(collector)
        onDispose { flow.remove(collector) }
    }
    return state
}
