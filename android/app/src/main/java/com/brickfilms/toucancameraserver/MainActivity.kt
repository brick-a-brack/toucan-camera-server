package com.brickfilms.toucancameraserver

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
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
import com.brickfilms.toucancameraserver.ui.server.ServerStatus
import com.brickfilms.toucancameraserver.ui.server.ServerUiState
import com.brickfilms.toucancameraserver.ui.theme.ToucanTheme
import java.net.InetAddress
import java.nio.ByteOrder
import kotlin.random.Random

class MainActivity : ComponentActivity() {

    private var onPermissionGranted: (() -> Unit)? = null

    private val permissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { grants ->
            if (grants[Manifest.permission.CAMERA] == true) {
                onPermissionGranted?.invoke()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        setContent {
            ToucanTheme {
                var uiState by remember {
                    mutableStateOf(
                        ServerUiState(
                            status = if (CameraServerService.isRunning) ServerStatus.Running else ServerStatus.Idle,
                            address = getWifiIpAddress() ?: "–",
                            token = loadOrCreateToken(),
                        )
                    )
                }

                ServerScreen(
                    state = uiState,
                    onToggleServer = {
                        if (uiState.isRunning) {
                            stopService(Intent(this, CameraServerService::class.java))
                            uiState = uiState.copy(status = ServerStatus.Idle)
                        } else {
                            CameraServerService.setToken(uiState.token)
                            requestCameraAndStart {
                                uiState = uiState.copy(
                                    status = ServerStatus.Running,
                                    address = getWifiIpAddress() ?: "–",
                                )
                            }
                        }
                    },
                    onRegenerateToken = {
                        val newToken = generateToken()
                        saveToken(newToken)
                        CameraServerService.setToken(newToken)
                        uiState = uiState.copy(token = newToken)
                    },
                    onToggleTokenVisibility = {
                        uiState = uiState.copy(tokenHidden = !uiState.tokenHidden)
                    },
                    onCopy = { _, text ->
                        val clipboard = getSystemService(CLIPBOARD_SERVICE) as ClipboardManager
                        clipboard.setPrimaryClip(ClipData.newPlainText("", text))
                    },
                )
            }
        }
    }

    private fun requestCameraAndStart(onGranted: () -> Unit) {
        val hasCam = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
        if (hasCam) {
            doStartService()
            onGranted()
        } else {
            val permissions = buildList {
                add(Manifest.permission.CAMERA)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU)
                    add(Manifest.permission.POST_NOTIFICATIONS)
            }.toTypedArray()
            onPermissionGranted = {
                doStartService()
                onGranted()
            }
            permissionLauncher.launch(permissions)
        }
    }

    private fun doStartService() {
        val intent = Intent(this, CameraServerService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
    }

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
