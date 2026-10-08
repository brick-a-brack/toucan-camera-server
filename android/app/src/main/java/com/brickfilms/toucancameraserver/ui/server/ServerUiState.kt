package com.brickfilms.toucancameraserver.ui.server

import androidx.compose.runtime.Immutable
import com.brickfilms.toucancameraserver.ServerPhase
import com.brickfilms.toucancameraserver.ServerState

enum class ServerStatus { Idle, Starting, Running, Error }

@Immutable
data class ServerUiState(
    val status: ServerStatus = ServerStatus.Idle,
    /** LAN address of this device — the server binds `0.0.0.0`, which is not
     *  dialable, so the reachable address comes from the Wi-Fi interface. */
    val address: String = "–",
    /** The port actually bound, straight from the native status. */
    val port: Int = 0,
    val token: String = "TOUCAN",
    val tokenHidden: Boolean = false,
    val errorMessage: String? = null,
    val version: String = "",
    val backends: List<String> = emptyList(),
) {
    val isRunning: Boolean get() = status == ServerStatus.Running
    /** A toggle is in flight; the power button should not accept another tap. */
    val isBusy: Boolean get() = status == ServerStatus.Starting

    companion object {
        fun from(
            server: ServerState,
            lanAddress: String,
            fallbackToken: String,
            fallbackPort: Int,
            tokenHidden: Boolean,
        ): ServerUiState {
            return ServerUiState(
                status = when (server.phase) {
                    ServerPhase.Stopped  -> ServerStatus.Idle
                    ServerPhase.Starting -> ServerStatus.Starting
                    ServerPhase.Running  -> ServerStatus.Running
                    ServerPhase.Failed   -> ServerStatus.Error
                },
                address = lanAddress,
                port = if (server.port > 0) server.port else fallbackPort,
                token = server.token.ifEmpty { fallbackToken },
                tokenHidden = tokenHidden,
                errorMessage = server.error.takeIf { server.phase == ServerPhase.Failed },
                version = server.version,
                backends = server.backends,
            )
        }
    }
}
