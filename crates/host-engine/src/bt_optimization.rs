//! Bluetooth Coexistence & Automatic Video Lip-Sync / Remote Media Keys.

#[cfg(target_os = "linux")]
use std::process::Command;

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

/// Automatically informs Linux PipeWire/PulseAudio of the phone's downstream Bluetooth earbud delay
/// so web browsers (YouTube/Netflix) and video players (MPV/VLC) automatically delay video to match!
pub fn apply_automatic_video_lipsync_offset_ms(bt_delay_ms: u16) {
    #[cfg(target_os = "linux")]
    {
        // PipeWire/PulseAudio latency offset is specified in microseconds
        let offset_us = (bt_delay_ms as u64) * 1_000;
        let _ = Command::new("pactl")
            .args([
                "set-port-latency-offset",
                "AudioShare_Ultra",
                "analog-output",
                &offset_us.to_string(),
            ])
            .output();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = bt_delay_ms;
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
        // Try playerctl (MPRIS standard used by Chrome, Firefox, Spotify, VLC on Linux)
        if Command::new("playerctl").arg(action).output().is_err() {
            let key = match cmd {
                RemoteMediaCommand::PlayPause => "XF86AudioPlay",
                RemoteMediaCommand::NextTrack => "XF86AudioNext",
                RemoteMediaCommand::PrevTrack => "XF86AudioPrev",
                RemoteMediaCommand::None => return,
            };
            let _ = Command::new("xdotool").args(["key", key]).output();
        }
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

    #[test]
    fn test_remote_media_command() {
        assert_eq!(RemoteMediaCommand::from_u8(1), RemoteMediaCommand::PlayPause);
        assert_eq!(RemoteMediaCommand::from_u8(2), RemoteMediaCommand::NextTrack);
        assert_eq!(RemoteMediaCommand::from_u8(3), RemoteMediaCommand::PrevTrack);
        assert_eq!(RemoteMediaCommand::from_u8(0), RemoteMediaCommand::None);
        execute_remote_media_command(RemoteMediaCommand::None);
    }
}
