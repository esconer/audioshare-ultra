package com.audioshare.ultra.ui

import android.net.Uri
import android.util.Size
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.core.content.ContextCompat
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Fully open-source embedded QR scanner: CameraX + ZXing Core, with no Google
 * Play Services dependency (F-Droid / GPL friendly).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun QrScannerDialog(
    onDismiss: () -> Unit,
    onScanned: (host: String, pin: String) -> Unit
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val handled = remember { AtomicBoolean(false) }
    val executor = remember { Executors.newSingleThreadExecutor() }

    DisposableEffect(Unit) {
        onDispose { executor.shutdownNow() }
    }

    Dialog(onDismissRequest = onDismiss) {
        StudioCard {
            Text("Scan PC Pairing QR", style = MaterialTheme.typography.titleMedium)
            Text(
                "Point at the QR code in AudioShare Ultra on your PC.",
                style = MaterialTheme.typography.bodySmall,
                color = StudioColors.Muted
            )
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(260.dp)
                    .clip(RoundedCornerShape(12.dp)),
                contentAlignment = Alignment.Center
            ) {
                AndroidView(
                    modifier = Modifier.fillMaxSize(),
                    factory = { ctx ->
                        val previewView = PreviewView(ctx)
                        val reader = MultiFormatReader().apply {
                            setHints(mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE)))
                        }
                        val providerFuture = ProcessCameraProvider.getInstance(ctx)
                        providerFuture.addListener({
                            runCatching {
                                val provider = providerFuture.get()
                                val preview = Preview.Builder().build().also {
                                    it.setSurfaceProvider(previewView.surfaceProvider)
                                }
                                @Suppress("DEPRECATION")
                                val analysis = ImageAnalysis.Builder()
                                    .setTargetResolution(Size(960, 720))
                                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                                    .build()
                                analysis.setAnalyzer(executor) { proxy ->
                                    try {
                                        if (handled.get()) return@setAnalyzer
                                        val yPlane = proxy.planes[0]
                                        val yBuf = yPlane.buffer
                                        val bytes = ByteArray(yBuf.remaining())
                                        yBuf.get(bytes)
                                        val src = PlanarYUVLuminanceSource(
                                            bytes, proxy.width, proxy.height,
                                            0, 0, proxy.width, proxy.height, false
                                        )
                                        val text = reader.decodeWithState(
                                            BinaryBitmap(HybridBinarizer(src))
                                        ).text
                                        val uri = Uri.parse(text)
                                        if (uri.scheme == "asultra" && uri.host == "connect") {
                                            val host = uri.getQueryParameter("host").orEmpty()
                                            val pin = uri.getQueryParameter("pin").orEmpty()
                                            if (host.isNotBlank() && handled.compareAndSet(false, true)) {
                                                ContextCompat.getMainExecutor(ctx).execute {
                                                    onScanned(host, pin)
                                                }
                                            }
                                        }
                                    } catch (_: Exception) {
                                        // Frame without a decodable QR code; ignore.
                                    } finally {
                                        reader.reset()
                                        proxy.close()
                                    }
                                }
                                provider.unbindAll()
                                provider.bindToLifecycle(
                                    lifecycleOwner,
                                    CameraSelector.DEFAULT_BACK_CAMERA,
                                    preview,
                                    analysis
                                )
                            }
                        }, ContextCompat.getMainExecutor(ctx))
                        previewView
                    }
                )
                // Minimal viewfinder corner frame.
                Canvas(modifier = Modifier.size(180.dp)) {
                    val len = 26.dp.toPx()
                    val stroke = 3.dp.toPx()
                    val c = StudioColors.AccentSoft
                    listOf(
                        Offset(0f, 0f) to Offset(len, 0f),
                        Offset(0f, 0f) to Offset(0f, len),
                        Offset(size.width, 0f) to Offset(size.width - len, 0f),
                        Offset(size.width, 0f) to Offset(size.width, len),
                        Offset(0f, size.height) to Offset(len, size.height),
                        Offset(0f, size.height) to Offset(0f, size.height - len),
                        Offset(size.width, size.height) to Offset(size.width - len, size.height),
                        Offset(size.width, size.height) to Offset(size.width, size.height - len)
                    ).forEach { (a, b) -> drawLine(c, a, b, stroke, StrokeCap.Round) }
                }
            }
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDismiss) { Text("Cancel") }
            }
        }
    }
}
