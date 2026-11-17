package dev.arl.audio.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import dev.arl.audio.R

/**
 * Pantalla única, sin estado propio. Un solo scroll, margen 24 dp, tope 480 dp.
 *
 * El micrófono de 120 dp es la acción dominante y el texto de debajo dice qué
 * hace: iniciar o detener la captura local. El estado del audio y el de la
 * conexión van aparte. La conexión no tiene botón: no hay nada que configurar.
 * El medidor de nivel aparece sólo con un valor real. La ganancia muestra lo
 * pedido y, aparte, lo efectivo o "no disponible"; mover el slider no pretende
 * cambiar el audio.
 */
@Composable
fun HomeScreen(
    state: HomeUiState,
    actions: HomeActions,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Column(
            modifier = Modifier.widthIn(max = 480.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Header(state)
            MicButton(state, actions)
            MicActionLabel(state)
            AudioStatus(state)
            ConnectionNote()
            if (state.capture == Capture.Local && state.levelAvailable) {
                LevelMeter(state.level)
            }
            if (state.capture == Capture.Local) {
                MuteRow(state, actions)
            }
            state.error?.let { ErrorCard(it, actions) }
            if (state.notificationWarning.isNotEmpty()) {
                NotificationWarning(state, actions)
            }
            AdvancedSection(state, actions)
        }
    }
}

@Composable
private fun Header(state: HomeUiState) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Text(
            text = stringResource(R.string.home_title),
            style = MaterialTheme.typography.headlineMedium,
        )
        if (state.versionName.isNotEmpty()) {
            Spacer(modifier = Modifier.height(4.dp))
            Text(
                text = stringResource(R.string.version_label, state.versionName),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/** Texto visible de la acción. El icono solo no dice qué va a pasar. */
@Composable
private fun MicActionLabel(state: HomeUiState) {
    Text(
        text = stringResource(
            if (state.capture == Capture.Local) R.string.action_stop_capture
            else R.string.action_start_capture,
        ),
        style = MaterialTheme.typography.titleMedium,
        modifier = Modifier.testTag("mic-label"),
    )
}

/** Estado del audio, sin mezclarlo con la conexión. */
@Composable
private fun AudioStatus(state: HomeUiState) {
    val label = stringResource(audioLabelOf(state))
    val icon = audioIconOf(state)
    Surface(
        shape = CircleShape,
        color = MaterialTheme.colorScheme.surfaceVariant,
        modifier = Modifier
            .semantics {
                contentDescription = label
                liveRegion = LiveRegionMode.Polite
            }
            .testTag("audio-status"),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(
                painter = painterResource(icon),
                contentDescription = null,
                modifier = Modifier.size(20.dp),
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(text = label, style = MaterialTheme.typography.labelLarge)
        }
    }
}

/**
 * La conexión no existe como producto. Se dice y no se ofrece un botón:
 * no hay nada que pulsar que la configure.
 */
@Composable
private fun ConnectionNote() {
    Text(
        text = stringResource(R.string.connection_unconfigured),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier
            .semantics { liveRegion = LiveRegionMode.Polite }
            .testTag("connection"),
    )
}

@Composable
private fun MicButton(state: HomeUiState, actions: HomeActions) {
    val capturing = state.capture == Capture.Local
    val description = stringResource(
        if (capturing) R.string.cd_mic_stop else R.string.cd_mic_start,
    )
    FilledIconButton(
        onClick = actions.onMicClick,
        modifier = Modifier
            .size(120.dp)
            .semantics { contentDescription = description }
            .testTag("mic"),
        shape = CircleShape,
    ) {
        Icon(
            painter = painterResource(R.drawable.ic_ui_mic),
            contentDescription = null,
            modifier = Modifier.size(48.dp),
        )
    }
}

@Composable
private fun LevelMeter(level: Float) {
    val clamped = level.coerceIn(0f, 1f)
    val percent = (clamped * 100).toInt()
    val description = stringResource(R.string.cd_level, percent)
    LinearProgressIndicator(
        progress = { clamped },
        modifier = Modifier
            .fillMaxWidth()
            .height(8.dp)
            .clip(MaterialTheme.shapes.small)
            .semantics { contentDescription = description }
            .testTag("level"),
    )
}

@Composable
private fun MuteRow(state: HomeUiState, actions: HomeActions) {
    val label = stringResource(
        if (state.muted) R.string.action_unmute else R.string.action_mute,
    )
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        FilledTonalIconToggleButton(
            checked = state.muted,
            onCheckedChange = { actions.onMuteClick() },
            modifier = Modifier
                .size(48.dp)
                .semantics { contentDescription = label }
                .testTag("mute"),
        ) {
            Icon(
                painter = painterResource(
                    if (state.muted) R.drawable.ic_ui_mic_off else R.drawable.ic_ui_mic,
                ),
                contentDescription = null,
            )
        }
        Text(text = label, style = MaterialTheme.typography.labelMedium)
        Text(
            text = stringResource(
                when {
                    state.capture != Capture.Local -> R.string.mute_requested_disconnected
                    state.muted -> R.string.mute_applied
                    else -> R.string.mute_not_applied
                },
            ),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.testTag("mute-honesty"),
        )
    }
}

@Composable
private fun ErrorCard(error: UiError, actions: HomeActions) {
    val message = stringResource(messageOf(error.kind))
    Card(
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        ),
        modifier = Modifier
            .fillMaxWidth()
            .semantics {
                contentDescription = message
                liveRegion = LiveRegionMode.Polite
            }
            .testTag("error"),
    ) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_ui_error),
                    contentDescription = null,
                    modifier = Modifier.size(24.dp),
                )
                Text(text = message, style = MaterialTheme.typography.bodyMedium)
            }
            error.action?.let { action ->
                Button(
                    onClick = { actions.onErrorAction(action) },
                    modifier = Modifier
                        .height(48.dp)
                        .testTag("error-action"),
                ) {
                    Text(text = stringResource(labelOf(action)))
                }
            }
        }
    }
}

/**
 * Aviso, no error. Denegar POST_NOTIFICATIONS no para la captura.
 * El botón pide el permiso si el sistema todavía lo muestra; si no, abre
 * los ajustes de notificaciones de la app.
 */
@Composable
private fun NotificationWarning(state: HomeUiState, actions: HomeActions) {
    val title = stringResource(R.string.notification_warning_title)
    val actionLabel = stringResource(
        if (state.notificationRequestable) R.string.action_allow_notifications
        else R.string.action_notification_settings,
    )
    Card(
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.secondaryContainer,
            contentColor = MaterialTheme.colorScheme.onSecondaryContainer,
        ),
        modifier = Modifier
            .fillMaxWidth()
            .semantics {
                contentDescription = title
                liveRegion = LiveRegionMode.Polite
            }
            .testTag("notification-warning"),
    ) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(text = title, style = MaterialTheme.typography.titleSmall)
            Text(text = state.notificationWarning, style = MaterialTheme.typography.bodyMedium)
            Button(
                onClick = actions.onNotificationAction,
                modifier = Modifier
                    .height(48.dp)
                    .testTag("notification-action"),
            ) {
                Text(text = actionLabel)
            }
        }
    }
}

@Composable
private fun AdvancedSection(state: HomeUiState, actions: HomeActions) {
    Column(modifier = Modifier.fillMaxWidth()) {
        HorizontalDivider()
        val toggleLabel = stringResource(
            if (state.advancedExpanded) R.string.advanced_hide else R.string.advanced_show,
        )
        TextButton(
            onClick = actions.onToggleAdvanced,
            modifier = Modifier
                .height(48.dp)
                .semantics { contentDescription = toggleLabel }
                .testTag("advanced-toggle"),
        ) {
            Icon(
                painter = painterResource(
                    if (state.advancedExpanded) R.drawable.ic_ui_expand_less
                    else R.drawable.ic_ui_expand_more,
                ),
                contentDescription = null,
            )
            Text(text = toggleLabel)
        }
        if (state.advancedExpanded) {
            GainBlock(state, actions)
        }
    }
}

@Composable
private fun GainBlock(state: HomeUiState, actions: HomeActions) {
    val requested = stringResource(R.string.gain_requested, state.gainRequestedDb)
    val effective = if (state.gainEffectiveDb == null) {
        stringResource(R.string.gain_effective_unavailable)
    } else {
        stringResource(R.string.gain_effective, state.gainEffectiveDb)
    }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(
            text = stringResource(R.string.gain_title),
            style = MaterialTheme.typography.titleSmall,
        )
        Text(
            text = stringResource(R.string.gain_honesty),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(text = requested, style = MaterialTheme.typography.bodyMedium)
        Text(text = effective, style = MaterialTheme.typography.bodyMedium)
        Slider(
            value = state.gainRequestedDb.toFloat(),
            onValueChange = { actions.onGainChange(it.toInt()) },
            valueRange = GAIN_MIN_DB.toFloat()..GAIN_MAX_DB.toFloat(),
            steps = GAIN_MAX_DB - GAIN_MIN_DB - 1,
            modifier = Modifier
                .fillMaxWidth()
                .semantics { contentDescription = requested }
                .testTag("gain"),
        )
    }
}

private fun audioLabelOf(state: HomeUiState): Int = when {
    state.error?.kind == ErrorKind.CaptureFailed -> R.string.status_error
    state.capture == Capture.Local -> R.string.status_local
    else -> R.string.status_idle
}

private fun audioIconOf(state: HomeUiState): Int = when {
    state.error?.kind == ErrorKind.CaptureFailed -> R.drawable.ic_ui_error
    state.capture == Capture.Local -> R.drawable.ic_ui_mic
    else -> R.drawable.ic_ui_mic_off
}

private fun messageOf(kind: ErrorKind): Int = when (kind) {
    ErrorKind.MicPermissionDenied -> R.string.error_mic_permission
    ErrorKind.CaptureFailed -> R.string.error_capture
}

private fun labelOf(action: ErrorAction): Int = when (action) {
    ErrorAction.GrantPermission -> R.string.action_grant
    ErrorAction.OpenSettings -> R.string.action_settings
    ErrorAction.Retry -> R.string.action_retry
}
