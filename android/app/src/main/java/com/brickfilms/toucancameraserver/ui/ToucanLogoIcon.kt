package com.brickfilms.toucancameraserver.ui

import androidx.compose.material3.Icon
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import com.brickfilms.toucancameraserver.R

@Composable
fun ToucanLogoIcon(
    @Suppress("UNUSED_PARAMETER") tint: Color = Color.Unspecified,
    modifier: Modifier = Modifier,
) {
    Icon(
        painter = painterResource(R.drawable.logo_toucan),
        contentDescription = "Toucan logo",
        tint = Color.Unspecified,
        modifier = modifier,
    )
}
