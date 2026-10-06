package com.brickfilms.toucancameraserver

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.*
import androidx.core.content.ContextCompat
import com.brickfilms.toucancameraserver.ui.server.ServerScreen
import com.brickfilms.toucancameraserver.ui.server.ServerUiState
import com.brickfilms.toucancameraserver.ui.theme.ToucanTheme
import kotlinx.coroutines.delay
import java.net.InetAddress
import java.nio.ByteOrder
import kotlin.random.Random

class MainActivity : ComponentActivity() {

    private var onPermissionGranted: (() -> Unit)? = null

    private val permissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { grants ->
            // The camera is required; notifications are not — without them the
            // server still runs, Android simply does not display the foreground
            // notification. `grants` only carries what was asked for this time,
            // so fall back to the current grant when the camera was not in it.
            val cameraGranted = grants[Manifest.permission.CAMERA] == true || hasPermission(
                Manifest.permission.CAMERA
            )
            if (cameraGranted) {
                onPermissionGranted?.invoke()
            }
            onPermissionGranted = null
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        setContent {
            ToucanTheme {
                // The native server is the source of truth: the screen renders what
                // it reports, so a server that is still binding, that failed, or that
                // landed on another port is shown as it really is.
                val server by CameraServerService.state.collectAsState()

                var token by remember { mutableStateOf(loadOrCreateToken()) }
                var tokenHidden by remember { mutableStateOf(false) }
                var address by remember { mutableStateOf(getWifiIpAddress() ?: "–") }

                // Resync on entry: the process may have outlived this activity with
                // the server still up (or the service may have stopped it).
                LaunchedEffect(Unit) { CameraServerService.refresh() }

                // While running, keep the status (and the uptime) fresh and notice a
                // stop that came from elsewhere, e.g. the notification being swiped.
                LaunchedEffect(server.phase) {
                    if (server.phase == ServerPhase.Running) {
                        address = getWifiIpAddress() ?: "–"
                        while (true) {
                            delay(2_000)
                            CameraServerService.refresh()
                        }
                    }
                }

                val uiState = ServerUiState.from(
                    server = server,
                    lanAddress = address,
                    fallbackToken = token,
                    fallbackPort = CameraServerService.DEFAULT_PORT,
                    tokenHidden = tokenHidden,
                )

                ServerScreen(
                    state = uiState,
                    onToggleServer = {
                        when (server.phase) {
                            // Starting: ignore the tap rather than racing the bind.
                            ServerPhase.Starting -> Unit
                            ServerPhase.Running  -> CameraServerService.stop(this)
                            // The server is meant to be driven from another
                            // device, so it always listens on the LAN.
                            else -> requestPermissionsAndStart {
                                CameraServerService.start(this, token)
                            }
                        }
                    },
                    onRegenerateToken = {
                        token = generateToken()
                        saveToken(token)
                        // Applies immediately, running or not.
                        CameraServerService.setToken(token)
                        CameraServerService.refresh()
                    },
                    onToggleTokenVisibility = { tokenHidden = !tokenHidden },
                    onCopy = { _, text ->
                        val clipboard = getSystemService(CLIPBOARD_SERVICE) as ClipboardManager
                        clipboard.setPrimaryClip(ClipData.newPlainText("", text))
                    },
                )
            }
        }
    }

    /**
     * Asks for whatever is still missing, then starts.
     *
     * Each permission is checked on its own: gating the whole request on the
     * camera meant POST_NOTIFICATIONS was never asked for again once the camera
     * had been granted, and on Android 13+ that silently costs the foreground
     * notification — `startForeground` succeeds and nothing is displayed.
     */
    private fun requestPermissionsAndStart(onGranted: () -> Unit) {
        val missing = buildList {
            if (!hasPermission(Manifest.permission.CAMERA)) {
                add(Manifest.permission.CAMERA)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
                !hasPermission(Manifest.permission.POST_NOTIFICATIONS)
            ) {
                add(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        if (missing.isEmpty()) {
            onGranted()
            return
        }
        onPermissionGranted = onGranted
        permissionLauncher.launch(missing.toTypedArray())
    }

    private fun hasPermission(permission: String): Boolean =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    private fun getWifiIpAddress(): String? {
        @Suppress("DEPRECATION")
        val wifiMgr = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        @Suppress("DEPRECATION")
        val ip = wifiMgr.connectionInfo?.ipAddress ?: return null
        if (ip == 0) return null
        val bytes = if (ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN) {
            byteArrayOf(
                (ip and 0xFF).toByte(),
                (ip shr 8 and 0xFF).toByte(),
                (ip shr 16 and 0xFF).toByte(),
                (ip shr 24 and 0xFF).toByte(),
            )
        } else {
            byteArrayOf(
                (ip shr 24 and 0xFF).toByte(),
                (ip shr 16 and 0xFF).toByte(),
                (ip shr 8 and 0xFF).toByte(),
                (ip and 0xFF).toByte(),
            )
        }
        return InetAddress.getByAddress(bytes).hostAddress
    }

    private fun generateToken(): String {
        val chars = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
        return (1..6).map { chars[Random.nextInt(chars.length)] }.joinToString("")
    }

    private fun loadOrCreateToken(): String {
        val prefs = getSharedPreferences("toucan", Context.MODE_PRIVATE)
        return prefs.getString("pairing_token", null) ?: generateToken().also { saveToken(it) }
    }

    private fun saveToken(token: String) {
        getSharedPreferences("toucan", Context.MODE_PRIVATE)
            .edit()
            .putString("pairing_token", token)
            .apply()
    }
}
