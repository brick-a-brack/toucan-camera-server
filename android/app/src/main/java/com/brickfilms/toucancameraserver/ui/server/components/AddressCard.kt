package com.brickfilms.toucancameraserver.ui.server.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Wifi
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.theme.LocalAccent
import com.brickfilms.toucancameraserver.ui.theme.SurfaceCardBorder
import com.brickfilms.toucancameraserver.ui.theme.SurfaceCardDim
import com.brickfilms.toucancameraserver.ui.theme.ToucanFg
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgFaint
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgMuted

@Composable
fun AddressCard(
    address: String,
    port: Int,
    onCopy: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val accent = LocalAccent.current
    val url = "http://$address:$port"

    Row(
        modifier = modifier
            .clip(RoundedCornerShape(18.dp))
            .background(SurfaceCardDim)
            .border(0.5.dp, SurfaceCardBorder, RoundedCornerShape(18.dp))
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Box(
            Modifier
                .size(36.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(Brush.linearGradient(listOf(accent.wash, Color(0x05FFFFFF))))
                .border(0.5.dp, accent.wash, RoundedCornerShape(12.dp)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                imageVector = Icons.Outlined.Wifi,
                contentDescription = null,
                tint = accent.primary,
                modifier = Modifier.size(18.dp),
            )
        }

        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(
                "LOCAL ADDRESS",
                color = ToucanFgFaint,
                style = MaterialTheme.typography.labelMedium,
            )
            Text(
                text = buildAnnotatedString {
                    withStyle(SpanStyle(color = ToucanFg)) { append(address) }
                    withStyle(SpanStyle(color = accent.primary)) { append(":$port") }
                },
                style = MaterialTheme.typography.bodyLarge,
                maxLines = 1,
            )
        }

        Box(
            Modifier
                .size(36.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(Color(0x08FFFFFF))
                .border(0.5.dp, SurfaceCardBorder, RoundedCornerShape(12.dp))
                .clickable { onCopy(url) },
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                imageVector = Icons.Outlined.ContentCopy,
                contentDescription = "Copy address",
                tint = ToucanFgMuted,
                modifier = Modifier.size(18.dp),
            )
        }
    }
}
