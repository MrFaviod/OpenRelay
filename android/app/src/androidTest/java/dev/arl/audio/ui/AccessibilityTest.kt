package dev.arl.audio.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.unit.Density
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test

/**
 * La pantalla dice la verdad del estado que recibe y no pinta un éxito.
 * Corre en emulador API 34: semántica de controles, scroll presente y fuente
 * grande sin que el texto de estado desaparezca.
 */
class AccessibilityTest {

    @get:Rule
    val rule = createComposeRule()

    @Test
    fun idle_says_no_connection_and_hides_level_and_mute() {
        rule.setContent {
            ArlTheme {
                HomeScreen(state = HomeUiState(), actions = HomeActions())
            }
        }
        rule.onNodeWithText("Captura parada").assertIsDisplayed()
        rule.onNodeWithText("Iniciar captura local").assertIsDisplayed()
        rule.onNodeWithText("La conexión al PC aún no está configurada.").assertIsDisplayed()
        rule.onNodeWithTag("mic").assertIsDisplayed()
        rule.onNodeWithTag("level").assertDoesNotExist()
        rule.onNodeWithTag("mute").assertDoesNotExist()
        rule.onNodeWithText("Sin conexión").assertDoesNotExist()
        rule.onNodeWithText("Emparejar").assertDoesNotExist()
        rule.onNodeWithText("Transmitiendo").assertDoesNotExist()
        rule.onNodeWithText("Configurar").assertDoesNotExist()
    }

    @Test
    fun local_capture_shows_meter_only_when_a_real_level_exists() {
        rule.setContent {
            ArlTheme {
                HomeScreen(
                    state = HomeUiState(capture = Capture.Local, levelAvailable = false),
                    actions = HomeActions(),
                )
            }
        }
        rule.onNodeWithText("Captura local").assertIsDisplayed()
        rule.onNodeWithTag("level").assertDoesNotExist()
        rule.onNodeWithTag("mute").assertIsDisplayed()
    }

    @Test
    fun mute_description_tracks_the_actual_toggle() {
        rule.setContent {
            ArlTheme {
                var muted by remember { mutableStateOf(false) }
                HomeScreen(
                    state = HomeUiState(capture = Capture.Local, muted = muted),
                    actions = HomeActions(onMuteClick = { muted = !muted }),
                )
            }
        }
        rule.onNodeWithText("Silencio desactivado. El nivel muestra la entrada del micrófono.")
            .performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("Las muestras capturadas se descartan. El nivel publicado queda en cero; el micrófono sigue abierto.")
            .assertDoesNotExist()
        rule.onNodeWithTag("mute").performScrollTo().performClick()
        rule.onNodeWithText("Las muestras capturadas se descartan. El nivel publicado queda en cero; el micrófono sigue abierto.")
            .performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("Silencio desactivado. El nivel muestra la entrada del micrófono.")
            .assertDoesNotExist()
    }

    @Test
    fun error_offers_one_action_and_does_not_claim_a_link() {
        var acted: ErrorAction? = null
        rule.setContent {
            ArlTheme {
                HomeScreen(
                    state = HomeUiState(
                        error = UiError(ErrorKind.MicPermissionDenied, ErrorAction.GrantPermission),
                    ),
                    actions = HomeActions(onErrorAction = { acted = it }),
                )
            }
        }
        rule.onNodeWithText("Error de captura").assertDoesNotExist()
        rule.onNodeWithText("El micrófono está denegado. Sin ese permiso no hay captura.").assertIsDisplayed()
        rule.onNodeWithText("Conceder permiso").assertIsDisplayed().performClick()
        rule.runOnIdle { assertEquals(ErrorAction.GrantPermission, acted) }
    }

    @Test
    fun advanced_starts_folded_and_open_shows_gain_unavailable() {
        rule.setContent {
            ArlTheme {
                var state by remember { mutableStateOf(HomeUiState()) }
                HomeScreen(
                    state = state,
                    actions = HomeActions(
                        onToggleAdvanced = {
                            state = reduce(state, HomeEvent.ToggleAdvanced)
                        },
                        onGainChange = { db ->
                            state = reduce(state, HomeEvent.SetGainRequested(db))
                        },
                    ),
                )
            }
        }
        rule.onNodeWithText("Efectiva: no disponible").assertDoesNotExist()
        rule.onNodeWithTag("advanced-toggle").performClick()
        rule.onNodeWithText("Efectiva: no disponible").assertIsDisplayed()
        rule.onNodeWithText("Mover esto guarda lo que pides. No cambia el audio.").assertIsDisplayed()
    }

    @Test
    fun large_font_keeps_status_text_inside_one_scroll() {
        rule.setContent {
            val base = LocalDensity.current
            androidx.compose.runtime.CompositionLocalProvider(
                LocalDensity provides Density(base.density, fontScale = 2f),
            ) {
                ArlTheme {
                    HomeScreen(state = HomeUiState(), actions = HomeActions())
                }
            }
        }
        rule.onNodeWithText("Captura parada").performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("Iniciar captura local").performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("La conexión al PC aún no está configurada.")
            .performScrollTo()
            .assertIsDisplayed()
    }
}
