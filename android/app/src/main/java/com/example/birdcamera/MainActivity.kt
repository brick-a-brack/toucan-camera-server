package com.brickfilms.toucancameraserver

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.ColorStateList
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.view.View
import android.widget.Button
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import java.net.InetAddress
import java.nio.ByteOrder

class MainActivity : AppCompatActivity() {

    private val requiredPermissions = buildList {
        add(Manifest.permission.CAMERA)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU)
            add(Manifest.permission.POST_NOTIFICATIONS)
    }.toTypedArray()

    private val permissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { grants ->
            if (grants[Manifest.permission.CAMERA] == true) {
                startCameraService()
            } else {
                statusText.text = "Camera permission denied — cannot start server."
                updateUiState()
            }
        }

    private lateinit var statusDot: View
    private lateinit var statusLabel: TextView
    private lateinit var statusText: TextView
    private lateinit var startButton: Button
    private lateinit var stopButton: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        statusDot   = findViewById(R.id.status_dot)
        statusLabel = findViewById(R.id.status_label)
        statusText  = findViewById(R.id.status_text)
        startButton = findViewById(R.id.btn_start)
        stopButton  = findViewById(R.id.btn_stop)

        startButton.setOnClickListener { onStartClicked() }
        stopButton.setOnClickListener  { onStopClicked()  }

        updateUiState()
    }

    override fun onResume() {
        super.onResume()
        updateUiState()
    }

    private fun onStartClicked() {
        val hasCam = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
        if (hasCam) {
            startCameraService()
        } else {
            permissionLauncher.launch(requiredPermissions)
        }
    }

    private fun onStopClicked() {
        stopService(Intent(this, CameraServerService::class.java))
        CameraServerService.isRunning = false
        updateUiState()
    }

    private fun startCameraService() {
        val intent = Intent(this, CameraServerService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
        CameraServerService.isRunning = true
        updateUiState()
    }

    private fun updateUiState() {
        val running = CameraServerService.isRunning
        val ip = getWifiIpAddress()

        if (running) {
            val color = ContextCompat.getColor(this, R.color.status_running)
            statusDot.backgroundTintList = ColorStateList.valueOf(color)
            statusLabel.text = getString(R.string.status_running)
            statusLabel.setTextColor(color)
            statusText.text = if (ip != null)
                "API available at\nhttp://$ip:8040"
            else
                "Server running\n(connect to WiFi for LAN address)"
            startButton.isEnabled = false
            stopButton.isEnabled  = true
        } else {
            val color = ContextCompat.getColor(this, R.color.status_stopped)
            statusDot.backgroundTintList = ColorStateList.valueOf(color)
            statusLabel.text = getString(R.string.status_stopped)
            statusLabel.setTextColor(color)
            statusText.text = getString(R.string.status_idle)
            startButton.isEnabled = true
            stopButton.isEnabled  = false
        }
    }

    private fun getWifiIpAddress(): String? {
        val wifiMgr = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        val ip = wifiMgr.connectionInfo?.ipAddress ?: return null
        if (ip == 0) return null
        val bytes = if (ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN) {
            byteArrayOf(
                (ip and 0xFF).toByte(),
                (ip shr 8 and 0xFF).toByte(),
                (ip shr 16 and 0xFF).toByte(),
                (ip shr 24 and 0xFF).toByte()
            )
        } else {
            byteArrayOf(
                (ip shr 24 and 0xFF).toByte(),
                (ip shr 16 and 0xFF).toByte(),
                (ip shr 8 and 0xFF).toByte(),
                (ip and 0xFF).toByte()
            )
        }
        return InetAddress.getByAddress(bytes).hostAddress
    }
}
