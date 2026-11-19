package dev.openrelay.app.ui

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

/** Semilla índigo de respaldo para API < 31. Contraste de texto ≥ 4.5:1. */
private val Indigo = Color(0xFF4355B9)
private val OnIndigo = Color(0xFFFFFFFF)
private val IndigoContainer = Color(0xFFDEE0FF)
private val OnIndigoContainer = Color(0xFF00105C)
private val IndigoDark = Color(0xFFBBC3FF)
private val OnIndigoDark = Color(0xFF08218A)
private val IndigoContainerDark = Color(0xFF293CA0)
private val OnIndigoContainerDark = Color(0xFFDEE0FF)

private val FallbackLight = lightColorScheme(
    primary = Indigo,
    onPrimary = OnIndigo,
    primaryContainer = IndigoContainer,
    onPrimaryContainer = OnIndigoContainer,
    secondaryContainer = Color(0xFFE1E0F5),
    surface = Color(0xFFFBFAFF),
    surfaceVariant = Color(0xFFE3E1EC),
    onSurfaceVariant = Color(0xFF46464F),
    error = Color(0xFFBA1A1A),
    errorContainer = Color(0xFFFFDAD6),
    onErrorContainer = Color(0xFF410002),
)

private val FallbackDark = darkColorScheme(
    primary = IndigoDark,
    onPrimary = OnIndigoDark,
    primaryContainer = IndigoContainerDark,
    onPrimaryContainer = OnIndigoContainerDark,
    secondaryContainer = Color(0xFF444559),
    surface = Color(0xFF131318),
    surfaceVariant = Color(0xFF46464F),
    onSurfaceVariant = Color(0xFFC7C5D0),
    error = Color(0xFFFFB4AB),
    errorContainer = Color(0xFF93000A),
    onErrorContainer = Color(0xFFFFDAD6),
)

/**
 * Dynamic color en API ≥ 31 (claro/oscuro del sistema). Por debajo, índigo fijo.
 * Tipografía: la de Material3, que en Android resuelve a la del sistema.
 */
@Composable
fun OpenRelayTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val scheme = when {
        Build.VERSION.SDK_INT >= 31 && darkTheme -> dynamicDarkColorScheme(context)
        Build.VERSION.SDK_INT >= 31 -> dynamicLightColorScheme(context)
        darkTheme -> FallbackDark
        else -> FallbackLight
    }
    MaterialTheme(colorScheme = scheme, content = content)
}
