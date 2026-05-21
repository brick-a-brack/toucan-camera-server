package com.brickfilms.toucancameraserver.ui.server.components

import androidx.compose.animation.core.*
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Shield
import androidx.compose.material.icons.outlined.Visibility
import androidx.compose.material.icons.outlined.VisibilityOff
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.theme.LocalAccent
import com.brickfilms.toucancameraserver.ui.theme.ToucanFg
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgDim
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgMuted

@Composable
fun TokenCard(
    token: String,
    hidden: Boolean,
    onRegenerate: () -> Unit,
    onToggleHidden: () -> Unit,
    onCopy: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val accent = LocalAccent.current

    Box(
        modifier = modifier
            .clip(RoundedCornerShape(22.dp))
            .background(Brush.linearGradient(listOf(accent.wash, Color(0x05FFFFFF))))
            .border(0.5.dp, accent.wash, RoundedCornerShape(22.dp)),
    ) {
        AnimatedSweep(color = accent.glow)

        Column(Modifier.padding(horizontal = 18.dp, vertical = 16.dp)) {
            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Icon(
                        imageVector = Icons.Outlined.Shield,
                        contentDescription = null,
                        tint = accent.primary,
                        modifier = Modifier.size(16.dp),
                    )
                    Text(
                        "PAIRING TOKEN",
                        color = accent.primary,
                        style = MaterialTheme.typography.labelMedium,
                    )
                }
                Row(
                    Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable(onClick = onToggleHidden)
                        .padding(horizontal = 8.dp, vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Icon(
                        imageVector = if (hidden) Icons.Outlined.VisibilityOff else Icons.Outlined.Visibility,
                        contentDescription = null,
                        tint = ToucanFgDim,
                        modifier = Modifier.size(16.dp),
                    )
                    Text(
                        if (hidden) "SHOW" else "HIDE",
                        color = ToucanFgDim,
                        style = MaterialTheme.typography.labelSmall,
                    )
                }
            }

            Spacer(Modifier.height(14.dp))

            val display = if (hidden) "•".repeat(token.length.coerceAtLeast(6)) else token
            Text(
                text = display,
                color = ToucanFg,
                style = MaterialTheme.typography.headlineLarge,
                modifier = Modifier.fillMaxWidth(),
            )

            Spacer(Modifier.height(14.dp))

            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                SecondaryAction(
                    icon = Icons.Outlined.Refresh,
                    label = "Regenerate",
                    onClick = onRegenerate,
                    modifier = Modifier.weight(1f),
                )
                PrimaryIconChip(
                    icon = Icons.Outlined.ContentCopy,
                    onClick = { onCopy(token) },
                )
            }
        }
    }
}

@Composable
private fun SecondaryAction(
    icon: ImageVector,
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier = modifier
            .height(42.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(Color(0x0AFFFFFF))
            .border(0.5.dp, Color(0x14FFFFFF), RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .padding(horizontal = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Icon(icon, contentDescription = null, tint = ToucanFg, modifier = Modifier.size(18.dp))
        Text(
            label,
            color = ToucanFg,
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.weight(1f),
            textAlign = TextAlign.Center,
        )
    }
}

@Composable
private fun PrimaryIconChip(icon: ImageVector, onClick: () -> Unit) {
    val accent = LocalAccent.current
    Box(
        Modifier
            .size(42.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(Brush.linearGradient(listOf(accent.primary, accent.primaryDim)))
            .border(0.5.dp, accent.primary, RoundedCornerShape(12.dp))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(icon, contentDescription = null, tint = accent.onPrimary, modifier = Modifier.size(18.dp))
    }
}

@Composable
private fun AnimatedSweep(color: Color) {
    val phase by rememberInfiniteTransition(label = "sweep").animateFloat(
        initialValue = 0f, targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(4000, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "phase",
    )
    var widthPx by remember { mutableIntStateOf(0) }
    Box(
        Modifier
            .fillMaxSize()
            .onSizeChanged { widthPx = it.width },
    ) {
        Box(
            Modifier
                .fillMaxHeight()
                .fillMaxWidth(0.4f)
                .graphicsLayer {
                    translationX = (phase * widthPx * 2f) - widthPx * 0.4f
                }
                .background(
                    Brush.horizontalGradient(
                        colors = listOf(Color.Transparent, color.copy(alpha = 0.25f), Color.Transparent),
                    )
                )
        )
    }
}
