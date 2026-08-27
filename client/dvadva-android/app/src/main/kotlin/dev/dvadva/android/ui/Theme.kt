package dev.dvadva.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// The cyan of the mark (see the repo's assets/make_icon.py). A phone client is
// a dark-first tool; the light scheme exists for completeness.
private val Cyan = Color(0xFF22D3EE)
private val Teal = Color(0xFF0E7490)
private val DarkSurface = Color(0xFF0B1418)
private val DarkBackground = Color(0xFF081014)

private val DarkScheme = darkColorScheme(
    primary = Cyan,
    onPrimary = Color(0xFF00323C),
    secondary = Teal,
    background = DarkBackground,
    surface = DarkSurface,
)

private val LightScheme = lightColorScheme(
    primary = Teal,
    secondary = Cyan,
)

@Composable
fun DvaDvaTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = if (dark) DarkScheme else LightScheme,
        content = content,
    )
}
