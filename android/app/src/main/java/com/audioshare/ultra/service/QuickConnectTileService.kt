package com.audioshare.ultra.service

import android.content.Context
import android.content.Intent
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService

class QuickConnectTileService : TileService() {

    override fun onStartListening() {
        super.onStartListening()
        syncTileState()
    }

    override fun onClick() {
        super.onClick()
        val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
        val lastHost = prefs.getString("last_host_addr", "192.168.42.129:65530") ?: "192.168.42.129:65530"

        val intent = Intent(this, AudioEngineService::class.java)
        if (AudioEngineService.isStreaming) {
            intent.action = "ACTION_STOP"
            startService(intent)
        } else {
            intent.putExtra("HOST_ADDR", lastHost)
            intent.putExtra("PIN", prefs.getString("pin_code", "") ?: "")
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                startForegroundService(intent)
            } else {
                startService(intent)
            }
        }
        syncTileState()
    }

    private fun syncTileState() {
        val tile = qsTile ?: return
        if (AudioEngineService.isStreaming) {
            tile.state = Tile.STATE_ACTIVE
            tile.label = "PC Audio: ON"
        } else {
            tile.state = Tile.STATE_INACTIVE
            tile.label = "PC Audio"
        }
        tile.updateTile()
    }
}
