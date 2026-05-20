package com.example.birdcamera

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
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
            }
        }

    private lateinit var statusText: TextView
    private lateinit var startButton: Button
    private lateinit var stopButton: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        statusText  = findViewById(R.id.status_text)
        startButton = findViewById(R.id.btn_start)
        stopButton  = findViewById(R.id.btn_stop)

        startButton.setOnClickListener { onStartClicked() }
        stopButton.setOnClickListener  { onStopClicked()  }

        updateStatusText()
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
        statusText.text = "Server stopped."
    }

    private fun startCameraService() {
        val intent = Intent(this, CameraServerService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
        updateStatusText()
    }

    private fun updateStatusText() {
        val ip = getWifiIpAddress()
        statusText.text = if (ip != null)
            "Server starting…\nURL: http://$ip:8040"
        else
            "Server starting…\n(Connect to WiFi to get the LAN address)"
    }

    private fun getWifiIpAddress(): String? {
        val wifiMgr = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        val ip = wifiMgr.connectionInfo?.ipAddress ?: return null
        if (ip == 0) return null
        // Android gives the IP as a little-endian int on most devices
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
