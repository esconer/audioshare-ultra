package com.audioshare.ultra

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.ui.graphics.Color
import com.audioshare.ultra.service.AudioEngineService
import com.audioshare.ultra.ui.MainScreen

private val StudioDarkColors = darkColorScheme(
    primary = Color(0xFF4F8BFF),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFF2563EB),
    onPrimaryContainer = Color(0xFFFFFFFF),
    secondary = Color(0xFF38BDF8),
    secondaryContainer = Color(0xFF16253E),
    onSecondaryContainer = Color(0xFFE0F2FE),
    tertiary = Color(0xFF34D399),
    background = Color(0xFF070A10),
    onBackground = Color(0xFFF4F7FB),
    surface = Color(0xFF0E1420),
    onSurface = Color(0xFFF4F7FB),
    surfaceVariant = Color(0xFF151D2E),
    onSurfaceVariant = Color(0xFF8B9AB5),
    error = Color(0xFFF87171)
)

class MainActivity : ComponentActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null
    private val notificationPermLauncher = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { /* The foreground service still runs; granting just exposes its controls. */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        // Seamless edge-to-edge obsidian canvas behind status & gesture bars.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
        )
        super.onCreate(savedInstanceState)

        // Hardware volume rocker always adjusts media volume, even before a
        // native stream has opened an AAudio stream of its own.
        volumeControlStream = android.media.AudioManager.STREAM_MUSIC

        // targetSdk 34: the FGS notification controls (Mute / Play-Pause /
        // Disconnect) stay hidden until POST_NOTIFICATIONS is granted.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            notificationPermLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }

        // Allow Android Wi-Fi hardware filter to receive UDP Auto-Discovery Beacons (Port 65531)
        val wifiMgr = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        multicastLock = wifiMgr?.createMulticastLock("AudioShareUltra::DiscoveryBroadcastLock")?.apply {
            setReferenceCounted(false)
            if (!isHeld) acquire()
        }

        if (savedInstanceState == null) {
            handleConnectIntent(intent)
        }

        setContent {
            MaterialTheme(colorScheme = StudioDarkColors) {
                MainScreen()
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleConnectIntent(intent)
    }

    private fun handleConnectIntent(intent: Intent?) {
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
    }

    override fun onDestroy() {
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }
}
