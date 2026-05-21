package com.brickfilms.toucancameraserver.ui.server

import androidx.compose.runtime.Immutable

enum class ServerStatus { Idle, Running }

@Immutable
data class ServerUiState(
    val status: ServerStatus = ServerStatus.Idle,
    val address: String = "–",
    val port: Int = 8040,
    val token: String = "TOUCAN",
    val tokenHidden: Boolean = false,
) {
    val isRunning: Boolean get() = status == ServerStatus.Running
}
