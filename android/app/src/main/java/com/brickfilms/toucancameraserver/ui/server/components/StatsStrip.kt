package com.brickfilms.toucancameraserver.ui.server.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.theme.LocalAccent
import com.brickfilms.toucancameraserver.ui.theme.SurfaceCardBorder
import com.brickfilms.toucancameraserver.ui.theme.ToucanFg
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgFaint
import com.brickfilms.toucancameraserver.ui.theme.ToucanFgGhost
import kotlin.time.Duration

@Composable
fun StatsStrip(
    running: Boolean,
    uptime: Duration,
    clients: Int,
    bitrateMbps: Double,
    modifier: Modifier = Modifier,
) {
    val accent = LocalAccent.current
    val items = listOf(
        StatItem("UPTIME",  if (running) fmtUptime(uptime) else "–", "since started", false),
        StatItem("CLIENTS", if (running) clients.toString() else "0", "connected", false),
        StatItem("BITRATE", if (running && bitrateMbps > 0.0) "%.1f".format(bitrateMbps) else "–", "Mbit/s", true),
    )
    Row(
        modifier = modifier
            .background(Color.Transparent)
            .padding(vertical = 14.dp)
            .heightIn(min = 64.dp),
    ) {
        items.forEachIndexed { i, item ->
            if (i > 0) {
                Box(
                    Modifier
                        .width(0.5.dp)
                        .fillMaxHeight()
                        .background(SurfaceCardBorder)
                )
            }
            Column(
                Modifier.weight(1f),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Text(
                    text = item.value,
                    color = if (item.accent) accent.primary else ToucanFg,
                    style = MaterialTheme.typography.headlineSmall,
                )
                Text(
                    text = buildAnnotatedString {
                        append(item.label)
                        withStyle(SpanStyle(color = ToucanFgGhost)) { append(" · ${item.hint}") }
                    },
                    color = ToucanFgFaint,
                    style = MaterialTheme.typography.labelSmall,
                    textAlign = TextAlign.Center,
                )
            }
        }
    }
}

@Composable
fun StatsDivider(modifier: Modifier = Modifier) {
    Box(
        modifier
            .fillMaxWidth()
            .height(0.5.dp)
            .background(SurfaceCardBorder)
    )
}

private data class StatItem(
    val label: String,
    val value: String,
    val hint: String,
    val accent: Boolean,
)

private fun fmtUptime(d: Duration): String = d.toComponents { h, m, s, _ ->
    "%02d:%02d:%02d".format(h, m, s)
}
