//! Bluetooth Coexistence & Automatic Video Lip-Sync / Remote Media Keys.

#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU16, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMediaCommand {
    None = 0,
    PlayPause = 1,
    NextTrack = 2,
    PrevTrack = 3,
}

impl RemoteMediaCommand {
    pub fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::PlayPause,
            2 => Self::NextTrack,
            3 => Self::PrevTrack,
            _ => Self::None,
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LinuxSinkInfo {
    /// Numeric PipeWire node ID (`object.id`), required by `pw-cli s <id> Props`.
    node_id: Option<String>,
    /// PulseAudio/PipeWire card index (`Card: N`), required by `pactl set-port-latency-offset`.
    card_idx: Option<String>,
    active_port: Option<String>,
}

/// Parse `object.id`, `Card:`, and `Active Port:` for `target_sink` from
/// `pactl list sinks`. Never overwrite `Card:` with kernel `alsa.card = "..."`
/// because `pactl set-port-latency-offset` expects the Pulse/PipeWire card ID.
#[cfg(target_os = "linux")]
fn parse_sink_info(text: &str, target_sink: &str) -> Option<LinuxSinkInfo> {
    let mut in_target = false;
    let mut info = LinuxSinkInfo::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sink #") {
            if in_target {
                break;
            }
            info = LinuxSinkInfo::default();
        } else if let Some(name) = trimmed.strip_prefix("Name: ") {
            in_target = name.trim() == target_sink;
        } else if in_target {
            if let Some(id) = trimmed.strip_prefix("object.id = \"") {
                let id = id.trim_end_matches('"').trim();
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                    info.node_id = Some(id.to_string());
                }
            } else if let Some(card) = trimmed.strip_prefix("Card: ") {
                info.card_idx = Some(card.trim().to_string());
            } else if let Some(port) = trimmed.strip_prefix("Active Port: ") {
                info.active_port = Some(port.trim().to_string());
            } else if info.card_idx.is_none() {
                if let Some(card) = trimmed.strip_prefix("alsa.card = \"") {
                    info.card_idx = Some(card.trim_end_matches('"').to_string());
                }
            }
        }
    }
    if in_target
        && (info.node_id.is_some() || (info.card_idx.is_some() && info.active_port.is_some()))
    {
        Some(info)
    } else {
        None
    }
}

#[cfg(all(target_os = "linux", test))]
fn parse_default_sink_card_and_port(text: &str, default_sink: &str) -> Option<(String, String)> {
    let info = parse_sink_info(text, default_sink)?;
    Some((info.card_idx?, info.active_port?))
}

#[cfg(target_os = "linux")]
fn resolve_linux_lipsync_targets() -> Vec<LinuxSinkInfo> {
    let Ok(sinks_out) = Command::new("pactl").args(["list", "sinks"]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&sinks_out.stdout);
    let mut targets = Vec::new();
    if let Some(virt) = parse_sink_info(&text, "AudioShare_Ultra") {
        targets.push(virt);
    }
    if let Ok(def) = Command::new("pactl").arg("get-default-sink").output() {
        let default_sink = String::from_utf8_lossy(&def.stdout).trim().to_string();
        if !default_sink.is_empty() && default_sink != "AudioShare_Ultra" {
            if let Some(info) = parse_sink_info(&text, &default_sink) {
                targets.push(info);
            }
        }
    }
    targets
}

#[cfg(target_os = "linux")]
static LAST_LIPSYNC_REQUESTED: AtomicU16 = AtomicU16::new(u16::MAX);
#[cfg(target_os = "linux")]
static LIPSYNC_APPLY_LOCK: Mutex<()> = Mutex::new(());
#[cfg(target_os = "linux")]
static LIPSYNC_APPLIED_TARGETS: std::sync::OnceLock<Mutex<Vec<LinuxSinkInfo>>> =
    std::sync::OnceLock::new();

/// Minimum spacing between remote media subprocess launches. This is called
/// from the host's hard real-time TX loop; an unauthenticated LAN peer sending
/// hundreds of feedback packets per second must not be able to fork that many
/// `playerctl`/`xdotool` processes.
#[cfg(target_os = "linux")]
const MEDIA_CMD_MIN_INTERVAL: Duration = Duration::from_millis(150);
#[cfg(target_os = "linux")]
static LAST_MEDIA_CMD: Mutex<Option<Instant>> = Mutex::new(None);

#[cfg(target_os = "linux")]
fn media_command_due(last: Option<Instant>, now: Instant) -> bool {
    last.map_or(true, |prev| now.saturating_duration_since(prev) >= MEDIA_CMD_MIN_INTERVAL)
}

/// Apply the offset synchronously. Used by both the async normal path and the
/// exit-time synchronous reset (which must finish before the process dies).
#[cfg(target_os = "linux")]
fn apply_linux_lipsync_now(bt_delay_ms: u16) {
    let _guard = LIPSYNC_APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if LAST_LIPSYNC_REQUESTED.load(Ordering::Relaxed) != bt_delay_ms {
        return;
    }
    let offset_us = (bt_delay_ms as u64) * 1_000;
    let offset_ns = offset_us * 1_000;
    let mut targets = resolve_linux_lipsync_targets();
    let history = LIPSYNC_APPLIED_TARGETS.get_or_init(|| Mutex::new(Vec::new()));
    if bt_delay_ms == 0 {
        // Reset any sink we previously offset even if the default sink changed.
        for old in history.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            if !targets.contains(&old) {
                targets.push(old);
            }
        }
    } else {
        *history.lock().unwrap_or_else(|e| e.into_inner()) = targets.clone();
    }
    let prop = format!("{{ latencyOffsetNsec = {} }}", offset_ns);
    let us_str = offset_us.to_string();
    for sink in &targets {
        if let Some(ref node_id) = sink.node_id {
            let _ = Command::new("pw-cli")
                .args(["s", node_id, "Props", &prop])
                .output();
        }
        if let (Some(ref card), Some(ref port)) = (&sink.card_idx, &sink.active_port) {
            let _ = Command::new("pactl")
                .args(["set-port-latency-offset", card, port, &us_str])
                .output();
        }
    }
}

/// Automatically informs Linux PipeWire/PulseAudio of the phone's downstream
/// Bluetooth earbud delay so web browsers and video players delay video to match.
pub fn apply_automatic_video_lipsync_offset_ms(bt_delay_ms: u16) {
    #[cfg(target_os = "linux")]
    {
        // Coalesce repeated requests for the same delay.
        if LAST_LIPSYNC_REQUESTED.swap(bt_delay_ms, Ordering::Relaxed) == bt_delay_ms {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("as-ultra-bt-lipsync".into())
            .spawn(move || apply_linux_lipsync_now(bt_delay_ms));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = bt_delay_ms;
    }
}

/// Synchronously reset any applied PipeWire/PulseAudio latency offset to 0 ms.
/// Call during process shutdown so the OS cannot kill a detached reset thread.
pub fn reset_video_lipsync_offset_blocking() {
    #[cfg(target_os = "linux")]
    {
        LAST_LIPSYNC_REQUESTED.store(0, Ordering::Relaxed);
        apply_linux_lipsync_now(0);
    }
}

/// Executes Play/Pause/Next/Prev on the PC when the user taps their Bluetooth earbuds on the phone
pub fn execute_remote_media_command(cmd: RemoteMediaCommand) {
    if cmd == RemoteMediaCommand::None {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        let action = match cmd {
            RemoteMediaCommand::PlayPause => "play-pause",
            RemoteMediaCommand::NextTrack => "next",
            RemoteMediaCommand::PrevTrack => "previous",
            RemoteMediaCommand::None => return,
        };
        // Rate-limit BEFORE spawning a thread: the feedback packet carrying a
        // media command can be replayed hundreds of times per second by a
        // misbehaving LAN peer.
        {
            let mut last = LAST_MEDIA_CMD.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if !media_command_due(*last, now) {
                return;
            }
            *last = Some(now);
        }
        // This function is called from the host's hard real-time TX loop.
        // `playerctl` talks to D-Bus/MPRIS and can block for tens of
        // milliseconds, which would starve every connected client. Run the
        // subprocess on a detached helper thread instead.
        let _ = std::thread::Builder::new()
            .name("as-ultra-media-key".into())
            .spawn(move || {
                // Try playerctl (MPRIS standard used by Chrome, Firefox, Spotify, VLC).
                // Fall back to X11 keys whenever it is unavailable OR the
                // command fails (no active MPRIS player, D-Bus error, etc.).
                let mpris_ok = Command::new("playerctl")
                    .arg(action)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if !mpris_ok {
                    let key = match cmd {
                        RemoteMediaCommand::PlayPause => "XF86AudioPlay",
                        RemoteMediaCommand::NextTrack => "XF86AudioNext",
                        RemoteMediaCommand::PrevTrack => "XF86AudioPrev",
                        RemoteMediaCommand::None => return,
                    };
                    let _ = Command::new("xdotool").args(["key", key]).output();
                }
            });
    }

    #[cfg(target_os = "windows")]
    unsafe {
        #[link(name = "user32")]
        extern "system" {
            fn keybd_event(b_vk: u8, b_scan: u8, dw_flags: u32, dw_extra_info: usize);
        }
        const VK_MEDIA_NEXT_TRACK: u8 = 0xB0;
        const VK_MEDIA_PREV_TRACK: u8 = 0xB1;
        const VK_MEDIA_PLAY_PAUSE: u8 = 0xB3;
        const KEYEVENTF_KEYUP: u32 = 0x0002;

        let vk = match cmd {
            RemoteMediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
            RemoteMediaCommand::NextTrack => VK_MEDIA_NEXT_TRACK,
            RemoteMediaCommand::PrevTrack => VK_MEDIA_PREV_TRACK,
            RemoteMediaCommand::None => return,
        };
        keybd_event(vk, 0, 0, 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sink_card_parser_accepts_bluez_card_line() {
        let listing = "\
Sink #0
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tCard: 0
\tActive Port: analog-output-lineout

Sink #1
\tState: RUNNING
\tName: bluez_sink.AA_BB_CC_DD_EE_FF.a2dp_sink
\tCard: 3
\tActive Port: a2dp-sink
";
        // BlueZ has no `alsa.card` property; `Card:` is the only card id.
        assert_eq!(
            parse_default_sink_card_and_port(listing, "bluez_sink.AA_BB_CC_DD_EE_FF.a2dp_sink"),
            Some(("3".to_string(), "a2dp-sink".to_string()))
        );
        assert_eq!(
            parse_default_sink_card_and_port(listing, "alsa_output.pci-0000_00_1f.3.analog-stereo"),
            Some(("0".to_string(), "analog-output-lineout".to_string()))
        );
        assert_eq!(parse_default_sink_card_and_port(listing, "missing"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sink_card_parser_prefers_pulse_card_index_and_reads_node_id() {
        let listing = "\
Sink #2
\tName: alsa_output.usb-Corsair
\tCard: 4
\tobject.id = \"56\"
\talsa.card = \"7\"
\tActive Port: analog-output
";
        let info = parse_sink_info(listing, "alsa_output.usb-Corsair").unwrap();
        assert_eq!(info.node_id.as_deref(), Some("56"));
        assert_eq!(
            parse_default_sink_card_and_port(listing, "alsa_output.usb-Corsair"),
            Some(("4".to_string(), "analog-output".to_string()))
        );
    }

    #[test]
    fn test_remote_media_command() {
        assert_eq!(
            RemoteMediaCommand::from_u8(1),
            RemoteMediaCommand::PlayPause
        );
        assert_eq!(
            RemoteMediaCommand::from_u8(2),
            RemoteMediaCommand::NextTrack
        );
        assert_eq!(
            RemoteMediaCommand::from_u8(3),
            RemoteMediaCommand::PrevTrack
        );
        assert_eq!(RemoteMediaCommand::from_u8(0), RemoteMediaCommand::None);
        execute_remote_media_command(RemoteMediaCommand::None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_remote_media_command_throttle_window() {
        let t = Instant::now();
        assert!(media_command_due(None, t));
        assert!(!media_command_due(Some(t), t + Duration::from_millis(149)));
        assert!(media_command_due(Some(t), t + Duration::from_millis(150)));
        assert!(media_command_due(
            Some(t),
            t + Duration::from_millis(1_000)
        ));
    }

    #[test]
    fn test_apply_automatic_video_lipsync_offset_ms_runs_without_panic() {
        apply_automatic_video_lipsync_offset_ms(0);
        apply_automatic_video_lipsync_offset_ms(145);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_synchronous_lipsync_reset_runs_without_panic() {
        // The exit path must complete synchronously (no detached thread
        // killed by process exit). Do not assert on the shared static: the
        // parallel apply-offset test writes the same global concurrently.
        reset_video_lipsync_offset_blocking();
    }
}
