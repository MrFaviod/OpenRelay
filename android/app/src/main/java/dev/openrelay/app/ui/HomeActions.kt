package dev.openrelay.app.ui

/**
 * Callbacks que la Activity (y, más adelante, el integrador) implementa.
 *
 * Contrato para el integrador — no llamar a una API que todavía no existe:
 * - [onMicClick]: si [HomeUiState.capture] es Idle, pedir RECORD_AUDIO y, sólo
 *   con el permiso concedido, arrancar la captura local del servicio. Si es
 *   Local, pararla. Nunca presentar el resultado como "conectado" o "transmitiendo".
 * - [onMuteClick]: alternar el mute del servicio. La UI sólo lo ofrece cuando
 *   la captura local está activa, porque mute sin captura no tiene efecto real.
 * - [onErrorAction]: GrantPermission relanza el pedido; OpenSettings abre los
 *   ajustes de la app; Retry reintenta la captura. No reintentar en bucle.
 * - [onGainChange]: persistir la preferencia local. No aplicar un efecto de
 *   audio que el motor no implementa; la ganancia efectiva sigue en null
 *   mientras no haya una medición real.
 * - [onNotificationAction]: si el pedido de POST_NOTIFICATIONS todavía es
 *   legal, lo lanza. Si ya se denegó, abre los ajustes de notificaciones.
 *   Denegar no para la captura.
 *
 * La Activity observa [dev.openrelay.app.StreamService] (`running`, `muted`,
 * `level`, `errorState`, `notificationWarning`) y lo traduce a [HomeUiState].
 */
data class HomeActions(
    val onMicClick: () -> Unit = {},
    val onMuteClick: () -> Unit = {},
    val onErrorAction: (ErrorAction) -> Unit = {},
    val onGainChange: (Int) -> Unit = {},
    val onToggleAdvanced: () -> Unit = {},
    val onNotificationAction: () -> Unit = {},
    /** Abre el escáner de QR (pide cámara sólo entonces). */
    val onScanQr: () -> Unit = {},
    /** Empareja con el URI pegado. */
    val onPairUri: (String) -> Unit = {},
    /** Borra la provisión local del PC. */
    val onForgetPc: () -> Unit = {},
)
