package dev.openrelay.app.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
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
            OpenRelayTheme {
                HomeScreen(state = HomeUiState(), actions = HomeActions())
            }
        }
        rule.onNodeWithContentDescription("Capture stopped").assertIsDisplayed()
        rule.onNodeWithContentDescription("Start local capture").assertIsDisplayed()
        rule.onNodeWithText("The PC connection is not set up yet.").assertIsDisplayed()
        rule.onNodeWithTag("mic").assertIsDisplayed()
        rule.onNodeWithTag("level").assertDoesNotExist()
        rule.onNodeWithTag("mute").assertDoesNotExist()
        rule.onNodeWithText("No connection").assertDoesNotExist()
        rule.onNodeWithText("Pair").assertDoesNotExist()
        rule.onNodeWithText("Transmitiendo").assertDoesNotExist()
        rule.onNodeWithText("Configurar").assertDoesNotExist()
    }

    @Test
    fun local_capture_shows_meter_only_when_a_real_level_exists() {
        rule.setContent {
            OpenRelayTheme {
                HomeScreen(
                    state = HomeUiState(capture = Capture.Local, levelAvailable = false),
                    actions = HomeActions(),
                )
            }
        }
        rule.onNodeWithContentDescription("Local capture").assertIsDisplayed()
        rule.onNodeWithTag("level").assertDoesNotExist()
        rule.onNodeWithTag("mute").assertIsDisplayed()
    }

    @Test
    fun mute_description_tracks_the_actual_toggle() {
        rule.setContent {
            OpenRelayTheme {
                var muted by remember { mutableStateOf(false) }
                HomeScreen(
                    state = HomeUiState(capture = Capture.Local, muted = muted),
                    actions = HomeActions(onMuteClick = { muted = !muted }),
                )
            }
        }
        rule.onNodeWithText("Mute is off. The level shows the microphone input.")
            .performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("Captured samples are discarded. The published level stays at zero; the microphone stays open.")
            .assertDoesNotExist()
        rule.onNodeWithTag("mute").performScrollTo().performClick()
        rule.onNodeWithText("Captured samples are discarded. The published level stays at zero; the microphone stays open.")
            .performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("Mute is off. The level shows the microphone input.")
            .assertDoesNotExist()
    }

    @Test
    fun error_offers_one_action_and_does_not_claim_a_link() {
        var acted: ErrorAction? = null
        rule.setContent {
            OpenRelayTheme {
                HomeScreen(
                    state = HomeUiState(
                        error = UiError(ErrorKind.MicPermissionDenied, ErrorAction.GrantPermission),
                    ),
                    actions = HomeActions(onErrorAction = { acted = it }),
                )
            }
        }
        rule.onNodeWithText("Capture error").assertDoesNotExist()
        rule.onNodeWithText("Microphone access is denied. Without that permission there is no capture.").assertIsDisplayed()
        rule.onNodeWithText("Grant permission").assertIsDisplayed().performClick()
        rule.runOnIdle { assertEquals(ErrorAction.GrantPermission, acted) }
    }

    @Test
    fun advanced_starts_folded_and_open_shows_gain_unavailable() {
        rule.setContent {
            OpenRelayTheme {
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
        rule.onNodeWithText("Effective: unavailable").assertDoesNotExist()
        rule.onNodeWithTag("advanced-toggle").performClick()
        rule.onNodeWithText("Effective: unavailable").assertIsDisplayed()
        rule.onNodeWithText("Moving this saves what you ask for. It does not change the audio.").assertIsDisplayed()
    }

    @Test
    fun each_control_has_one_talkback_stop() {
        rule.setContent {
            OpenRelayTheme {
                HomeScreen(
                    state = HomeUiState(capture = Capture.Local),
                    actions = HomeActions(),
                )
            }
        }
        rule.onAllNodesWithContentDescription("Stop capture").assertCountEquals(1)
        rule.onAllNodesWithText("Stop capture").assertCountEquals(0)
        rule.onAllNodesWithContentDescription("Local capture").assertCountEquals(1)
        rule.onAllNodesWithText("Local capture").assertCountEquals(0)
        rule.onAllNodesWithContentDescription("Mute").assertCountEquals(1)
        rule.onAllNodesWithText("Mute").assertCountEquals(0)
    }

    @Test
    fun large_font_keeps_status_text_inside_one_scroll() {
        rule.setContent {
            val base = LocalDensity.current
            androidx.compose.runtime.CompositionLocalProvider(
                LocalDensity provides Density(base.density, fontScale = 2f),
            ) {
                OpenRelayTheme {
                    HomeScreen(state = HomeUiState(), actions = HomeActions())
                }
            }
        }
        rule.onNodeWithContentDescription("Capture stopped").performScrollTo().assertIsDisplayed()
        rule.onNodeWithContentDescription("Start local capture").performScrollTo().assertIsDisplayed()
        rule.onNodeWithText("The PC connection is not set up yet.")
            .performScrollTo()
            .assertIsDisplayed()
    }
}
