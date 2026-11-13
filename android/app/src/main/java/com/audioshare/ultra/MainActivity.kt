package com.audioshare.ultra

import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.ui.graphics.Color
import com.audioshare.ultra.service.AudioEngineService
import com.audioshare.ultra.ui.MainScreen

private val StudioDarkColors = darkColorScheme(
    primary = Color(0xFF3B82F6),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFF1D4ED8),
    onPrimaryContainer = Color(0xFFFFFFFF),
    secondary = Color(0xFF38BDF8),
    secondaryContainer = Color(0xFF1E3A5F),
    onSecondaryContainer = Color(0xFFE0F2FE),
    tertiary = Color(0xFF34D399),
    background = Color(0xFF0A0D13),
    onBackground = Color(0xFFF1F5F9),
    surface = Color(0xFF111622),
    onSurface = Color(0xFFF1F5F9),
    surfaceVariant = Color(0xFF171E2E),
    onSurfaceVariant = Color(0xFF94A3B8),
    error = Color(0xFFF87171)
)

class MainActivity : ComponentActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Allow Android Wi-Fi hardware filter to receive UDP Auto-Discovery Beacons (Port 65531)
        val wifiMgr = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifiMgr?.createMulticastLock("AudioShareUltra::DiscoveryBroadcastLock")?.apply {
            setReferenceCounted(false)
            if (!isHeld) acquire()
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
            MaterialTheme(colorScheme = StudioDarkColors) {
                MainScreen()
            }
        }
    }

    override fun onDestroy() {
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }
}
