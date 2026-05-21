package com.brickfilms.toucancameraserver.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.staticCompositionLocalOf

val LocalAccent = staticCompositionLocalOf { ToucanAccent.Ember }

private val ToucanColorScheme = darkColorScheme(
    background   = ToucanBg,
    surface      = ToucanBg,
    onBackground = ToucanFg,
    onSurface    = ToucanFg,
)

@Composable
fun ToucanTheme(
    accent: ToucanAccent = ToucanAccent.Ember,
    content: @Composable () -> Unit,
) {
    CompositionLocalProvider(LocalAccent provides accent) {
        MaterialTheme(
            colorScheme = ToucanColorScheme,
            typography  = ToucanTypography,
            content     = content,
        )
    }
}
