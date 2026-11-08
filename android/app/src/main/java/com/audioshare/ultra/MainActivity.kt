package com.audioshare.ultra

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.core.content.ContextCompat
import com.audioshare.ultra.ui.MainScreen

class MainActivity : ComponentActivity() {
    private val recordAudioPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (ContextCompat.checkSelfPermission(
                this,
                Manifest.permission.RECORD_AUDIO
            ) != PackageManager.PERMISSION_GRANTED
        ) {
            recordAudioPermission.launch(Manifest.permission.RECORD_AUDIO)
        }
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                MainScreen()
            }
        }
    }
}
