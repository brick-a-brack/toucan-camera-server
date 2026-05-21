package com.brickfilms.toucancameraserver.ui.server

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import com.brickfilms.toucancameraserver.ui.server.components.*
import com.brickfilms.toucancameraserver.ui.theme.*

@Composable
fun ServerScreen(
    state: ServerUiState,
    onToggleServer: () -> Unit,
    onRegenerateToken: () -> Unit,
    onToggleTokenVisibility: () -> Unit,
    onCopy: (label: String, text: String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val accent = LocalAccent.current

    Box(
        modifier = modifier
            .fillMaxSize()
            .background(ToucanBg)
            .background(
                Brush.radialGradient(
                    colors = listOf(accent.wash.copy(alpha = 0.22f), Color.Transparent),
                    center = Offset(x = 500f, y = -50f),
                    radius = 1400f,
                )
            )
            .background(
                Brush.verticalGradient(
                    colors = listOf(ToucanBgGradTop, ToucanBgGradMid, ToucanBgGradBot),
                )
            )
            .windowInsetsPadding(WindowInsets.statusBars)
            .windowInsetsPadding(WindowInsets.navigationBars)
    ) {
        Column(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState()),
        ) {
            AppBar()

            Column(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 22.dp, vertical = 4.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Spacer(Modifier.height(2.dp))
                StatusHeader(running = state.isRunning)
                Spacer(Modifier.height(18.dp))
                PowerButton(running = state.isRunning, onToggle = onToggleServer)
                Spacer(Modifier.height(22.dp))
                HeroTagline(running = state.isRunning)
                Spacer(Modifier.height(14.dp))
            }

            Spacer(Modifier.height(18.dp))

            Column(
                Modifier
                    .fillMaxWidth()
                    .alpha(if (state.isRunning) 1f else 0.55f),
            ) {
                TokenCard(
                    token = state.token,
                    hidden = state.tokenHidden,
                    onRegenerate = onRegenerateToken,
                    onToggleHidden = onToggleTokenVisibility,
                    onCopy = { onCopy("Token", it) },
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 22.dp),
                )
                Spacer(Modifier.height(12.dp))
                AddressCard(
                    address = state.address,
                    port = state.port,
                    onCopy = { onCopy("Address", it) },
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 22.dp),
                )
            }

            Spacer(Modifier.height(18.dp))
        }
    }
}

@Composable
private fun AppBar() {
    Text(
        text = "Toucan Camera Server",
        color = ToucanFg,
        style = MaterialTheme.typography.titleMedium,
        textAlign = TextAlign.Center,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 22.dp, vertical = 18.dp),
    )
}

@Composable
private fun HeroTagline(running: Boolean) {
    val accent = LocalAccent.current
    val lineHeight = MaterialTheme.typography.displaySmall.lineHeight
    val boxHeight = with(LocalDensity.current) { lineHeight.toDp() * 2 }

    val text = buildAnnotatedString {
        if (running) {
            withStyle(SpanStyle(color = ToucanFg)) { append("Your camera, ") }
            withStyle(InstrumentSerifItalic.toSpanStyle().copy(color = accent.primary)) {
                append("broadcasting")
            }
        } else {
            withStyle(SpanStyle(color = ToucanFg)) { append("Tap to ") }
            withStyle(InstrumentSerifItalic.toSpanStyle().copy(color = accent.primary)) {
                append("go live")
            }
        }
    }
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(boxHeight),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = text,
            style = MaterialTheme.typography.displaySmall,
            textAlign = TextAlign.Center,
            maxLines = 2,
        )
    }
}
