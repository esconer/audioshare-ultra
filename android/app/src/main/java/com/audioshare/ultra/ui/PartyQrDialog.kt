package com.audioshare.ultra.ui

import android.graphics.Bitmap
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ContentCopy
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import java.net.Inet4Address
import java.net.NetworkInterface

data class PhoneNetEndpoint(
    val ifaceName: String,
    val ip: String,
    val isHotspot: Boolean
)

/**
 * Enumerate reachable local IPv4 addresses on this phone, prioritizing
 * Mobile Hotspot (`ap*`, `swlan*`, `softap*`, `192.168.43.*`) and Wi-Fi
 * (`wlan*`) while excluding unreachable cellular/VPN adapters (`rmnet*`,
 * `ccmni*`, `pdp*`, `tun*`, `ppp*`, `dummy*`).
 */
fun listReachablePartyEndpoints(): List<PhoneNetEndpoint> {
    val result = mutableListOf<PhoneNetEndpoint>()
    runCatching {
        val ifaces = NetworkInterface.getNetworkInterfaces() ?: return emptyList()
        for (iface in ifaces) {
            if (!iface.isUp || iface.isLoopback) continue
            val name = iface.name.lowercase()
            val skip = name.startsWith("rmnet") || name.startsWith("ccmni") ||
                name.startsWith("pdp") || name.startsWith("tun") ||
                name.startsWith("ppp") || name.startsWith("dummy") ||
                name.contains("rmnet")
            if (skip) continue
            for (addr in iface.inetAddresses) {
                if (addr is Inet4Address && !addr.isLoopbackAddress && !addr.isLinkLocalAddress) {
                    val ip = addr.hostAddress ?: continue
                    val isHotspot = name.startsWith("ap") || name.startsWith("swlan") ||
                        name.startsWith("softap") || ip.startsWith("192.168.43.")
                    result.add(PhoneNetEndpoint(iface.name, ip, isHotspot))
                }
            }
        }
    }
    return result.sortedByDescending {
        if (it.isHotspot) 2
        else if (it.ifaceName.lowercase().startsWith("wlan")) 1
        else 0
    }
}

private fun renderQrBitmap(uri: String, sizePx: Int = 512): Bitmap? = runCatching {
    val hints: Map<EncodeHintType, Any> = mapOf(EncodeHintType.MARGIN to 2)
    val matrix = QRCodeWriter().encode(uri, BarcodeFormat.QR_CODE, sizePx, sizePx, hints)
    val bmp = Bitmap.createBitmap(sizePx, sizePx, Bitmap.Config.ARGB_8888)
    val pixels = IntArray(sizePx * sizePx)
    for (y in 0 until sizePx) {
        val rowOff = y * sizePx
        for (x in 0 until sizePx) {
            pixels[rowOff + x] = if (matrix[x, y]) 0xFF000000.toInt() else 0xFFFFFFFF.toInt()
        }
    }
    bmp.setPixels(pixels, 0, sizePx, 0, 0, sizePx, sizePx)
    bmp
}.getOrNull()

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun PartyQrDialog(
    partyPort: Int,
    pinCode: String,
    onDismiss: () -> Unit,
    onCopied: () -> Unit
) {
    val clipboard = LocalClipboardManager.current
    val endpoints = remember { listReachablePartyEndpoints() }
    var selectedIdx by remember { mutableIntStateOf(0) }
    val activeEndpoint = endpoints.getOrNull(selectedIdx)
    val ip = activeEndpoint?.ip ?: "192.168.43.1"
    val uri = remember(ip, partyPort, pinCode) {
        val cleanPin = pinCode.trim()
        if (cleanPin.isNotEmpty()) "asultra://connect?host=$ip:$partyPort&pin=$cleanPin"
        else "asultra://connect?host=$ip:$partyPort"
    }
    val qrBitmap = remember(uri) { renderQrBitmap(uri) }

    Dialog(onDismissRequest = onDismiss) {
        StudioCard {
            Text(
                "Join Party via QR Code",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold
            )
            Text(
                "Friends can scan this from AudioShare Ultra even on routers that block discovery beacons.",
                style = MaterialTheme.typography.bodySmall,
                color = StudioColors.Muted
            )
            if (endpoints.size > 1) {
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    endpoints.forEachIndexed { idx, ep ->
                        FilterChip(
                            selected = selectedIdx == idx,
                            onClick = { selectedIdx = idx },
                            label = { Text("${if (ep.isHotspot) "Hotspot" else "Wi-Fi"} ${ep.ip}") }
                        )
                    }
                }
            }
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(vertical = 4.dp),
                contentAlignment = Alignment.Center
            ) {
                if (qrBitmap != null) {
                    Box(
                        modifier = Modifier
                            .size(224.dp)
                            .clip(RoundedCornerShape(16.dp))
                            .background(Color.White)
                            .padding(10.dp)
                    ) {
                        Image(
                            bitmap = qrBitmap.asImageBitmap(),
                            contentDescription = "Party Pairing QR Code",
                            modifier = Modifier.fillMaxSize()
                        )
                    }
                }
            }
            Text(
                uri,
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.labelSmall,
                color = StudioColors.AccentSoft
            )
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                OutlinedButton(onClick = {
                    clipboard.setText(AnnotatedString(uri))
                    onCopied()
                }) {
                    Icon(Icons.Rounded.ContentCopy, null, Modifier.size(15.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Copy Link")
                }
                TextButton(onClick = onDismiss) { Text("Done") }
            }
        }
    }
}
