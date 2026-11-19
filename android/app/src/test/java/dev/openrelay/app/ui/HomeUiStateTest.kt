package dev.openrelay.app.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test
import dev.openrelay.app.present

/**
 * El modelo no puede afirmar conexión, transmisión ni una ganancia aplicada.
 * Estos tests cubren esa honestidad sin Android.
 */
class HomeUiStateTest {

    @Test
    fun idle_starts_disconnected_without_level_or_effective_gain() {
        val state = HomeUiState()
        assertEquals(Capture.Idle, state.capture)
        assertEquals(Connection.Unavailable, state.connection)
        assertFalse(state.levelAvailable)
        assertNull(state.gainEffectiveDb)
        assertNull(state.error)
        assertFalse(state.muted)
    }

    @Test
    fun gain_request_is_clamped_and_does_not_invent_an_effective_value() {
        val high = reduce(HomeUiState(), HomeEvent.SetGainRequested(40))
        val low = reduce(HomeUiState(), HomeEvent.SetGainRequested(-40))
        assertEquals(GAIN_MAX_DB, high.gainRequestedDb)
        assertEquals(GAIN_MIN_DB, low.gainRequestedDb)
        assertNull(high.gainEffectiveDb)
        assertEquals(Connection.Unavailable, high.connection)
    }

    @Test
    fun advanced_and_mute_toggle_without_touching_capture() {
        val open = reduce(HomeUiState(), HomeEvent.ToggleAdvanced)
        val muted = reduce(open, HomeEvent.ToggleMute)
        assertFalse(HomeUiState().advancedExpanded)
        assertEquals(true, open.advancedExpanded)
        assertEquals(true, muted.muted)
        assertEquals(Capture.Idle, muted.capture)
    }

    @Test
    fun connection_enum_has_no_connected_state() {
        assertEquals(listOf(Connection.Unavailable), Connection.entries)
    }

    @Test
    fun present_maps_the_service_without_inventing_a_link_or_a_gain() {
        val shown = present(
            local = HomeUiState(gainRequestedDb = 3),
            running = true,
            muted = true,
            level = 0.5f,
            errorText = null,
            micGranted = true,
        )
        assertEquals(Capture.Local, shown.capture)
        assertEquals(Connection.Unavailable, shown.connection)
        assertEquals(true, shown.muted)
        assertEquals(true, shown.levelAvailable)
        assertNull(shown.gainEffectiveDb)
        assertEquals(3, shown.gainRequestedDb)

        val failed = present(
            HomeUiState(),
            running = false,
            muted = false,
            level = 0f,
            errorText = "start -1",
            micGranted = true,
        )
        assertEquals(ErrorKind.CaptureFailed, failed.error?.kind)
        assertEquals(Capture.Idle, failed.capture)
    }

    @Test
    fun present_drops_a_stuck_mic_denial_when_permission_returns_or_capture_runs() {
        val denied = HomeUiState(
            error = UiError(ErrorKind.MicPermissionDenied, ErrorAction.OpenSettings),
        )
        val stillDenied = present(
            denied,
            running = false,
            muted = false,
            level = 0f,
            errorText = null,
            micGranted = false,
        )
        assertEquals(ErrorKind.MicPermissionDenied, stillDenied.error?.kind)

        val granted = present(
            denied,
            running = false,
            muted = false,
            level = 0f,
            errorText = null,
            micGranted = true,
        )
        assertNull(granted.error)

        val capturing = present(denied, running = true, muted = false, level = 0f, errorText = null, micGranted = false)
        assertNull(capturing.error)
        assertEquals(Capture.Local, capturing.capture)
    }
}
