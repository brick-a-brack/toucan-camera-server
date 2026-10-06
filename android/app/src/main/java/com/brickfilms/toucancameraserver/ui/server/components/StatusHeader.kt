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
import com.brickfilms.toucancameraserver.ui.server.ServerStatus
import com.brickfilms.toucancameraserver.ui.theme.ErrorRed
import com.brickfilms.toucancameraserver.ui.theme.ErrorRedDot
import com.brickfilms.toucancameraserver.ui.theme.LiveGreen
import com.brickfilms.toucancameraserver.ui.theme.LiveGreenDot
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgDim
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgFaint

@Composable
fun StatusHeader(status: ServerStatus, modifier: Modifier = Modifier) {
    val label = when (status) {
        ServerStatus.Running  -> "LIVE · STREAMING"
        ServerStatus.Starting -> "STARTING · PLEASE WAIT"
        ServerStatus.Error    -> "ERROR · SERVER STOPPED"
        ServerStatus.Idle     -> "IDLE · SERVER STOPPED"
    }
    val color = when (status) {
        ServerStatus.Running -> LiveGreen
        ServerStatus.Error   -> ErrorRed
        else                 -> ToucanFgDim
    }

    Row(
        modifier = modifier,
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        // Fixed 20dp box so every state occupies the same height
        Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) {
            when (status) {
                ServerStatus.Running  -> PulsingDot(color = LiveGreenDot, periodMillis = 2000)
                ServerStatus.Starting -> PulsingDot(color = ToucanFgDim, periodMillis = 900)
                ServerStatus.Error    -> StaticDot(color = ErrorRedDot)
                ServerStatus.Idle     -> StaticDot(color = ToucanFgFaint)
            }
        }
        Text(
            text = label,
            color = color,
            style = MaterialTheme.typography.labelMedium,
        )
    }
}

@Composable
private fun StaticDot(color: Color) {
    Box(
        Modifier
            .size(8.dp)
            .clip(CircleShape)
            .background(color)
    )
}

@Composable
private fun PulsingDot(color: Color, periodMillis: Int) {
    val infinite = rememberInfiniteTransition(label = "live-dot")
    val ringAlpha by infinite.animateFloat(
        initialValue = 0.6f, targetValue = 0f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = periodMillis, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "ring-alpha",
    )
    val ringScale by infinite.animateFloat(
        initialValue = 1f, targetValue = 3.2f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = periodMillis, easing = FastOutSlowInEasing),
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
