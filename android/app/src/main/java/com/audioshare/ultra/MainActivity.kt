package com.audioshare.ultra

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.core.content.ContextCompat
import com.audioshare.ultra.service.AudioEngineService
import com.audioshare.ultra.ui.MainScreen

class MainActivity : ComponentActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null
    private val recordAudioPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Allow Android Wi-Fi hardware filter to receive UDP Auto-Discovery Beacons (Port 65531)
        val wifiMgr = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifiMgr?.createMulticastLock("AudioShareUltra::DiscoveryBroadcastLock")?.apply {
            setReferenceCounted(false)
            if (!isHeld) acquire()
        }

        if (ContextCompat.checkSelfPermission(
                this,
                Manifest.permission.RECORD_AUDIO
            ) != PackageManager.PERMISSION_GRANTED
        ) {
            recordAudioPermission.launch(Manifest.permission.RECORD_AUDIO)
        }

        // Check if launched via 1-Scan QR Code (asultra://connect?host=IP:PORT&pin=PIN)
        intent?.data?.let { uri ->
            if (uri.scheme == "asultra" && uri.host == "connect") {
                val scannedHost = uri.getQueryParameter("host")
                val scannedPin = uri.getQueryParameter("pin") ?: ""
                if (!scannedHost.isNullOrBlank()) {
                    getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
                        .edit()
                        .putString("last_host_addr", scannedHost)
                        .putString("pin_code", scannedPin)
                        .apply()

                    val svcIntent = Intent(this, AudioEngineService::class.java).apply {
                        putExtra("HOST_ADDR", scannedHost)
                        putExtra("PIN", scannedPin)
                    }
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        startForegroundService(svcIntent)
                    } else {
                        startService(svcIntent)
                    }
                }
            }
        }

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                MainScreen()
            }
        }
    }

    override fun onDestroy() {
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }
}
