package dev.arl.audio.ui

import androidx.compose.runtime.saveable.Saver

/**
 * Persistencia de proceso para lo único que esta Activity posee: el pliegue de
 * avanzados, la ganancia pedida y el mute pedido. Captura, conexión, nivel y
 * error no se guardan: los publica el servicio, y fingirlos tras una rotación
 * sería un estado falso.
 */
val HomeUiStateSaver: Saver<HomeUiState, List<Any>> = Saver(
    save = { listOf(it.advancedExpanded, it.gainRequestedDb, it.muted) },
    restore = { saved ->
        HomeUiState(
            advancedExpanded = saved[0] as Boolean,
            gainRequestedDb = clampGain(saved[1] as Int),
            muted = saved[2] as Boolean,
            connection = Connection.Unavailable,
        )
    },
)
