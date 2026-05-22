package com.brickfilms.toucancameraserver.ui.server.components

import androidx.compose.animation.core.*
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.theme.LiveGreen
import com.brickfilms.toucancameraserver.ui.theme.LiveGreenDot
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgDim
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgFaint

@Composable
fun StatusHeader(running: Boolean, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier,
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        // Fixed 20dp box so both states occupy the same height
        Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) {
            if (running) {
                PulsingDot(color = LiveGreenDot)
            } else {
                Box(
                    Modifier
                        .size(8.dp)
                        .clip(CircleShape)
                        .background(ToucanFgFaint)
                )
            }
        }
        Text(
            text = if (running) "LIVE · STREAMING" else "IDLE · SERVER STOPPED",
            color = if (running) LiveGreen else ToucanFgDim,
            style = MaterialTheme.typography.labelMedium,
        )
    }
}

@Composable
private fun PulsingDot(color: Color) {
    val infinite = rememberInfiniteTransition(label = "live-dot")
    val ringAlpha by infinite.animateFloat(
        initialValue = 0.6f, targetValue = 0f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 2000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "ring-alpha",
    )
    val ringScale by infinite.animateFloat(
        initialValue = 1f, targetValue = 3.2f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 2000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "ring-scale",
    )
    Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) {
        Box(
            Modifier
                .size(8.dp * ringScale)
                .clip(CircleShape)
                .background(color.copy(alpha = ringAlpha))
        )
        Box(
            Modifier
                .size(8.dp)
                .clip(CircleShape)
                .background(color)
        )
    }
}
