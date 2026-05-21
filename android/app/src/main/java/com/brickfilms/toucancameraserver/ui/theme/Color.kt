package com.brickfilms.toucancameraserver.ui.theme

import androidx.compose.runtime.Immutable
import androidx.compose.ui.graphics.Color

val ToucanBg          = Color(0xFF0A0807)
val ToucanBgGradTop   = Color(0xFF1A120A)
val ToucanBgGradMid   = Color(0xFF08060A)
val ToucanBgGradBot   = Color(0xFF06050C)

val ToucanFg          = Color(0xFFF5EFE7)
val ToucanFgMuted     = Color(0xB3F5EFE7)
val ToucanFgDim       = Color(0x8CF5EFE7)
val ToucanFgFaint     = Color(0x73F5EFE7)
val ToucanFgGhost     = Color(0x4DF5EFE7)

val SurfaceCard       = Color(0x0AFFFFFF)
val SurfaceCardDim    = Color(0x06FFFFFF)
val SurfaceCardBorder = Color(0x0FFFFFFF)
val Hairline          = Color(0x14FFFFFF)

val LiveGreen         = Color(0xFFBDF5D1)
val LiveGreenDot      = Color(0xFF4ADE80)

@Immutable
data class ToucanAccent(
    val primary: Color,
    val primaryDim: Color,
    val onPrimary: Color,
    val glow: Color,
    val wash: Color,
) {
    companion object {
        val Ember = ToucanAccent(
            primary    = Color(0xFFF39312),
            primaryDim = Color(0xFFC77407),
            onPrimary  = Color(0xFF1A0F04),
            glow       = Color(0x59F39312),
            wash       = Color(0x2EF39312),
        )
        val Magma = ToucanAccent(
            primary    = Color(0xFFFF5B3A),
            primaryDim = Color(0xFFB83A22),
            onPrimary  = Color(0xFF1A0904),
            glow       = Color(0x59FF5B3A),
            wash       = Color(0x2EFF5B3A),
        )
        val Citrus = ToucanAccent(
            primary    = Color(0xFFFFC247),
            primaryDim = Color(0xFFC99012),
            onPrimary  = Color(0xFF1A1004),
            glow       = Color(0x59FFC247),
            wash       = Color(0x2EFFC247),
        )
        val Mint = ToucanAccent(
            primary    = Color(0xFF5BE0B2),
            primaryDim = Color(0xFF1F8F69),
            onPrimary  = Color(0xFF04190F),
            glow       = Color(0x595BE0B2),
            wash       = Color(0x2E5BE0B2),
        )
        val Violet = ToucanAccent(
            primary    = Color(0xFFB58CFF),
            primaryDim = Color(0xFF7A5AE0),
            onPrimary  = Color(0xFF0F0419),
            glow       = Color(0x59B58CFF),
            wash       = Color(0x2EB58CFF),
        )
    }
}
