package com.brickfilms.toucancameraserver.ui.server

import androidx.compose.runtime.Immutable
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds

enum class ServerStatus { Idle, Running }

@Immutable
data class ServerUiState(
    val status: ServerStatus = ServerStatus.Idle,
    val address: String = "–",
    val port: Int = 8040,
    val token: String = "TOUCAN",
    val tokenHidden: Boolean = false,
    val uptime: Duration = Duration.ZERO,
    val clients: Int = 0,
    val bitrateMbps: Double = 0.0,
) {
    val isRunning: Boolean get() = status == ServerStatus.Running
}
