package com.audioshare.ultra.ui

import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlin.math.abs
import kotlin.math.sin

/** Studio palette: calm tonal layering instead of heavy 1dp blue-gray outlines. */
object StudioColors {
    val Canvas = Color(0xFF070A10)
    val CardBg = Color(0xFF101624)
    val CardBorder = Color(0x18FFFFFF) // 9% white hairline
    val TileBg = Color(0xFF0A0E17)
    val TileBorder = Color(0x12FFFFFF)
    val Accent = Color(0xFF4F8BFF)
    val AccentDeep = Color(0xFF162847)
    val AccentSoft = Color(0xFF93C5FD)
    val OkBg = Color(0xFF14532D)
    val OkFg = Color(0xFF86EFAC)
    val LiveGlow = Color(0xFF10B981)
    val WarnBg = Color(0xFF422006)
    val WarnFg = Color(0xFFFDE047)
    val Danger = Color(0xFFB91C1C)
    val DangerSoft = Color(0xFFFCA5A5)
    val Muted = Color(0xFF8B9AB5)
}

@Composable
fun SectionEyebrow(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text.uppercase(),
        style = MaterialTheme.typography.labelSmall.copy(
            letterSpacing = 1.4.sp,
            fontWeight = FontWeight.SemiBold
        ),
        color = StudioColors.Muted,
        modifier = modifier.padding(start = 4.dp, top = 4.dp)
    )
}

@Composable
fun StudioCard(
    modifier: Modifier = Modifier,
    borderColor: Color = StudioColors.CardBorder,
    content: @Composable ColumnScope.() -> Unit
) {
    Surface(
        modifier = modifier.fillMaxWidth(),
        shape = RoundedCornerShape(22.dp),
        color = StudioColors.CardBg,
        border = BorderStroke(1.dp, borderColor)
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            content = content
        )
    }
}

/**
 * Atmospheric Hero Audio Stage with a subtle audio-reactive radial glow:
 * emerald when live, cobalt on standby, amber/red on warning.
 */
@Composable
fun HeroAudioStage(
    glowColor: Color,
    energyFraction: Float,
    modifier: Modifier = Modifier,
    content: @Composable ColumnScope.() -> Unit
) {
    val smoothEnergy by animateFloatAsState(
        targetValue = energyFraction.coerceIn(0f, 1f),
        animationSpec = spring(stiffness = Spring.StiffnessMediumLow),
        label = "heroGlow"
    )
    val shape = RoundedCornerShape(26.dp)
    Box(
        modifier = modifier
            .fillMaxWidth()
            .clip(shape)
            .background(StudioColors.CardBg)
            .drawBehind {
                drawRect(
                    brush = Brush.radialGradient(
                        colors = listOf(
                            glowColor.copy(alpha = 0.14f + 0.14f * smoothEnergy),
                            Color.Transparent
                        ),
                        radius = 520f
                    )
                )
            }
            .border(1.dp, glowColor.copy(alpha = 0.24f), shape)
    ) {
        Column(
            modifier = Modifier.padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
            content = content
        )
    }
}

/** Fluid 60 FPS spring-smoothed symmetric audio-level bar visualizer. */
@Composable
fun FluidAudioVisualizer(vuPct: Int, isLive: Boolean, modifier: Modifier = Modifier) {
    val smoothLevel by animateFloatAsState(
        targetValue = if (isLive) (vuPct / 100f).coerceIn(0.04f, 1f) else 0.04f,
        animationSpec = spring(dampingRatio = 0.72f, stiffness = Spring.StiffnessMedium),
        label = "vuSmooth"
    )
    Canvas(
        modifier = modifier
            .fillMaxWidth()
            .height(28.dp)
    ) {
        val bars = 28
        val gap = 4.dp.toPx()
        val barW = ((size.width - gap * (bars - 1)) / bars).coerceAtLeast(2f)
        val center = (bars - 1) / 2f
        for (i in 0 until bars) {
            val dist = abs(i - center) / center // 0 at center, 1 at edges
            val envelope = (1f - dist * dist * 0.75f).coerceIn(0.18f, 1f)
            val ripple = 0.85f + 0.15f * sin(i * 1.3f + smoothLevel * 9f)
            val h = (size.height * smoothLevel * envelope * ripple).coerceAtLeast(4.dp.toPx())
            val y = (size.height - h) / 2f
            val color = when {
                !isLive -> Color(0xFF1F2A3E)
                smoothLevel > 0.88f && dist < 0.35f -> Color(0xFFF59E0B)
                else -> Color(0xFF34D399).copy(alpha = 0.45f + 0.55f * envelope)
            }
            drawRoundRect(
                color = color,
                topLeft = Offset(i * (barW + gap), y),
                size = Size(barW, h),
                cornerRadius = CornerRadius(barW / 2f, barW / 2f)
            )
        }
    }
}

@Composable
fun PulsingStatusDot(active: Boolean, color: Color) {
    val alpha = if (active) {
        val transition = rememberInfiniteTransition(label = "liveDot")
        val a by transition.animateFloat(
            initialValue = 1.0f,
            targetValue = 0.35f,
            animationSpec = infiniteRepeatable(tween(900), RepeatMode.Reverse),
            label = "dotAlpha"
        )
        a
    } else 1.0f
    Canvas(modifier = Modifier.size(9.dp)) {
        drawCircle(color = color.copy(alpha = alpha))
    }
}

@Composable
fun StatusBadge(
    text: String,
    icon: ImageVector? = null,
    containerColor: Color = Color(0xFF1A2436),
    contentColor: Color = Color(0xFFCBD5E1),
    onClick: (() -> Unit)? = null
) {
    Surface(
        shape = RoundedCornerShape(8.dp),
        color = containerColor,
        contentColor = contentColor,
        border = BorderStroke(1.dp, Color(0xFF2B3852)),
        modifier = if (onClick != null) Modifier.clickable { onClick() } else Modifier
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp)
        ) {
            if (icon != null) {
                Icon(icon, contentDescription = null, modifier = Modifier.size(13.dp))
            }
            Text(
                text = text,
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.SemiBold
            )
        }
    }
}

@Composable
fun MetricTile(label: String, value: String, modifier: Modifier = Modifier) {
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(10.dp),
        color = StudioColors.TileBg,
        border = BorderStroke(1.dp, StudioColors.TileBorder)
    ) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Text(label, style = MaterialTheme.typography.labelSmall, color = StudioColors.Muted)
            Text(
                value,
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.SemiBold
            )
        }
    }
}

@Composable
fun ModePresetCard(
    title: String,
    subtitle: String,
    badge: String,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier
) {
    Surface(
        modifier = modifier.clickable { onClick() },
        shape = RoundedCornerShape(12.dp),
        color = if (selected) Color(0xFF172544) else StudioColors.TileBg,
        border = BorderStroke(
            if (selected) 1.5.dp else 1.dp,
            if (selected) StudioColors.Accent else StudioColors.TileBorder
        )
    ) {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp)
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text(
                    title,
                    style = MaterialTheme.typography.bodyMedium,
                    fontWeight = FontWeight.Bold,
                    color = if (selected) Color.White else Color(0xFFE2E8F0)
                )
                StatusBadge(
                    text = badge,
                    containerColor = if (selected) StudioColors.Accent else Color(0xFF1A2233),
                    contentColor = if (selected) Color.White else StudioColors.Muted
                )
            }
            Text(subtitle, style = MaterialTheme.typography.labelSmall, color = StudioColors.Muted)
        }
    }
}

@Composable
fun SettingSwitchRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    enabled: Boolean = true,
    badgeText: String? = null,
    onCheckedChange: (Boolean) -> Unit
) {
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = StudioColors.TileBg,
        border = BorderStroke(1.dp, if (checked) StudioColors.Accent else StudioColors.TileBorder),
        modifier = Modifier.fillMaxWidth()
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Column(
                modifier = Modifier
                    .weight(1f)
                    .padding(end = 10.dp),
                verticalArrangement = Arrangement.spacedBy(2.dp)
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Text(title, style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.SemiBold)
                    if (badgeText != null) {
                        StatusBadge(
                            badgeText,
                            containerColor = StudioColors.OkBg,
                            contentColor = StudioColors.OkFg
                        )
                    }
                }
                Text(subtitle, style = MaterialTheme.typography.bodySmall, color = StudioColors.Muted)
            }
            Switch(checked = checked, enabled = enabled, onCheckedChange = onCheckedChange)
        }
    }
}

@Composable
fun ProDrawerHeader(
    title: String,
    badge: String,
    expanded: Boolean,
    onToggle: () -> Unit
) {
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = if (expanded) Color(0xFF172238) else StudioColors.CardBg,
        border = BorderStroke(1.dp, if (expanded) StudioColors.Accent else StudioColors.CardBorder),
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onToggle() }
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 14.dp, vertical = 12.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Row(
                modifier = Modifier.weight(1f),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp)
            ) {
                Icon(
                    imageVector = if (expanded) Icons.Rounded.ExpandMore else Icons.Rounded.ChevronRight,
                    contentDescription = null,
                    tint = StudioColors.AccentSoft,
                    modifier = Modifier.size(20.dp)
                )
                Text(title, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
            }
            StatusBadge(badge, containerColor = Color(0xFF1E293B), contentColor = StudioColors.AccentSoft)
        }
    }
}

@Composable
fun StudioFaderRow(
    title: String,
    freqLabel: String,
    value: Float,
    valueRange: ClosedFloatingPointRange<Float>,
    onValueChange: (Float) -> Unit,
    onValueChangeFinished: (() -> Unit)? = null
) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(title, style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium)
                if (freqLabel.isNotEmpty()) StatusBadge(freqLabel)
            }
            val boosted = abs(value) >= 0.05f
            StatusBadge(
                text = "%+.1f dB".format(value),
                containerColor = if (boosted) StudioColors.AccentDeep else Color(0xFF1A2436),
                contentColor = if (boosted) StudioColors.AccentSoft else StudioColors.Muted,
                onClick = {
                    onValueChange(0f)
                    onValueChangeFinished?.invoke()
                }
            )
        }
        Slider(
            value = value,
            onValueChange = onValueChange,
            onValueChangeFinished = onValueChangeFinished,
            valueRange = valueRange
        )
    }
}
