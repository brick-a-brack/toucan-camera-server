package com.brickfilms.toucancameraserver.ui.server.components

import androidx.compose.animation.core.*
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.PowerSettingsNew
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.theme.LocalAccent
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgMuted

@Composable
fun PowerButton(
    running: Boolean,
    onToggle: () -> Unit,
    modifier: Modifier = Modifier,
    size: Dp = 178.dp,
) {
    val accent = LocalAccent.current

    val rot by rememberInfiniteTransition(label = "halo-rot").animateFloat(
        initialValue = 0f, targetValue = 360f,
        animationSpec = infiniteRepeatable(
            animation = tween(14_000, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "rot",
    )
    val glowAlpha by rememberInfiniteTransition(label = "halo-pulse").animateFloat(
        initialValue = 0.65f, targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(3_000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "pulse",
    )

    Box(
        modifier = modifier.size(size),
        contentAlignment = Alignment.Center,
    ) {
        if (running) {
            Box(
                Modifier
                    .requiredSize(size + 24.dp)
                    .blur(20.dp)
                    .background(
                        Brush.radialGradient(
                            colors = listOf(
                                accent.glow.copy(alpha = accent.glow.alpha * glowAlpha),
                                Color.Transparent,
                            ),
                            radius = size.value * 1.2f,
                        ),
                        shape = CircleShape,
                    )
            )
        }

        Canvas(
            modifier = Modifier
                .matchParentSize()
                .rotate(rot)
                .padding(4.dp),
        ) {
            val r = size.toPx() / 2f
            val center = Offset(this.size.width / 2f, this.size.height / 2f)
            drawCircle(
                brush = Brush.sweepGradient(
                    0f to accent.primary.copy(alpha = 0f),
                    0.55f to accent.primary.copy(alpha = 0.15f),
                    0.85f to accent.primary.copy(alpha = if (running) 1f else 0.35f),
                    1f to accent.primary.copy(alpha = 0f),
                    center = center,
                ),
                radius = r - 6f,
                center = center,
                style = Stroke(width = 2.2f),
            )
            drawCircle(
                brush = Brush.sweepGradient(
                    0f to accent.primary.copy(alpha = 0f),
                    0.5f to accent.primary.copy(alpha = if (running) 0.5f else 0.2f),
                    1f to accent.primary.copy(alpha = 0f),
                    center = center,
                ),
                radius = r - 18f,
                center = center,
                style = Stroke(
                    width = 1.2f,
                    pathEffect = PathEffect.dashPathEffect(floatArrayOf(4f, 8f)),
                ),
            )
        }

        val coreBrush = if (running) {
            Brush.radialGradient(
                colors = listOf(accent.primary, accent.primaryDim, accent.primaryDim),
                center = Offset(0.3f * size.value, 0.2f * size.value),
                radius = size.value * 1.2f,
            )
        } else {
            Brush.radialGradient(
                colors = listOf(Color(0xFF1E1812), Color(0xFF110D09)),
                radius = size.value * 1.2f,
            )
        }
        val coreContent = if (running) accent.onPrimary else ToucanFgMuted

        Box(
            modifier = Modifier
                .padding(24.dp)
                .matchParentSize()
                .shadow(
                    elevation = if (running) 18.dp else 8.dp,
                    shape = CircleShape,
                    ambientColor = accent.glow,
                    spotColor = accent.glow,
                )
                .clip(CircleShape)
                .background(coreBrush)
                .clickable(onClick = onToggle),
            contentAlignment = Alignment.Center,
        ) {
            Column(
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Icon(
                    imageVector = Icons.Outlined.PowerSettingsNew,
                    contentDescription = if (running) "Stop server" else "Start server",
                    tint = coreContent,
                    modifier = Modifier.size(36.dp),
                )
                Text(
                    text = if (running) "TAP TO STOP" else "TAP TO START",
                    color = coreContent.copy(alpha = 0.85f),
                    style = MaterialTheme.typography.labelMedium,
                )
            }
        }
    }
}
