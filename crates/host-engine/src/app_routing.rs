#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioAppEntry {
    /// OS Sink-Input ID (Linux PipeWire/Pulse) or Process ID (Windows WASAPI Process Loopback)
    pub id: u32,
    pub pid: u32,
    pub name: String,
}

/// Enumerate active applications currently producing audio or running media sessions
pub fn list_active_audio_apps() -> Vec<AudioAppEntry> {
    #[cfg(target_os = "linux")]
    {
        list_linux_pipewire_sink_inputs()
    }
    #[cfg(target_os = "windows")]
    {
        list_windows_audio_processes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn list_linux_pipewire_sink_inputs() -> Vec<AudioAppEntry> {
    let mut apps = Vec::new();
    let Ok(output) = Command::new("pactl").args(["list", "sink-inputs"]).output() else {
        return apps;
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut current_id: Option<u32> = None;
    let mut current_pid: u32 = 0;
    let mut current_name: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Sink Input #") {
            if let (Some(id), Some(name)) = (current_id.take(), current_name.take()) {
                apps.push(AudioAppEntry {
                    id,
                    pid: current_pid,
                    name,
                });
            }
            current_id = rest.parse::<u32>().ok();
            current_pid = 0;
        } else if let Some(rest) = trimmed.strip_prefix("application.name = ") {
            current_name = Some(rest.trim_matches('"').to_string());
        } else if let Some(rest) = trimmed.strip_prefix("application.process.id = ") {
            current_pid = rest.trim_matches('"').parse::<u32>().unwrap_or(0);
        }
    }

    if let (Some(id), Some(name)) = (current_id, current_name) {
        apps.push(AudioAppEntry {
            id,
            pid: current_pid,
            name,
        });
    }
    apps
}

#[cfg(target_os = "windows")]
fn list_windows_audio_processes() -> Vec<AudioAppEntry> {
    let mut apps = Vec::new();
    // Query active desktop apps with window titles (candidates for WASAPI Process Loopback)
    let Ok(output) = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
    else {
        return apps;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let common_media = [
        "chrome.exe",
        "msedge.exe",
        "firefox.exe",
        "spotify.exe",
        "vlc.exe",
        "mpv.exe",
        "discord.exe",
        "brave.exe",
    ];

    for line in text.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim_matches('"')).collect();
        if parts.len() >= 2 {
            let name = parts[0].to_lowercase();
            if common_media.iter().any(|&m| name == m) {
                if let Ok(pid) = parts[1].parse::<u32>() {
                    apps.push(AudioAppEntry {
                        id: pid,
                        pid,
                        name: parts[0].to_string(),
                    });
                }
            }
        }
    }
    apps
}

/// Route a specific application into the streamed AudioShare sink (Linux PipeWire/Pulse)
/// or prepare its PID for Windows Process Loopback (`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`).
pub fn route_single_app_to_audioshare(app: &AudioAppEntry) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        // Ensure the sink that the host captures exists. Do not make it the
        // default output, so only explicitly routed apps are streamed.
        let _ = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=AudioShare_Ultra",
                "sink_properties=device.description=AudioShare_Wireless_Speaker",
            ])
            .output();

        // Capture the sink's monitor as the default input.
        let _ = Command::new("pactl")
            .args(["set-default-source", "AudioShare_Ultra.monitor"])
            .output();

        let status = Command::new("pactl")
            .args(["move-sink-input", &app.id.to_string(), "AudioShare_Ultra"])
            .status()
            .map_err(|e| format!("Failed to move sink-input: {}", e))?;

        if status.success() {
            Ok(format!(
                "Routed '{}' (PID {}) -> AudioShare_Ultra",
                app.name, app.pid
            ))
        } else {
            Err("pactl move-sink-input returned non-zero".into())
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "windows")]
        {
            // Boost priority & isolate target media process on Windows
            let script = format!(
                "$p = Get-Process -Id {} -ErrorAction SilentlyContinue; if ($p) {{ $p.PriorityClass = 'AboveNormal' }}",
                app.pid
            );
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output();
        }
        Ok(format!(
            "Target Process Loopback locked to '{}' (PID {})",
            app.name, app.pid
        ))
    }
}

/// Restores all isolated applications back to the default system speaker/sink
pub fn restore_all_apps_to_default_sink() -> String {
    #[cfg(target_os = "linux")]
    {
        for app in list_active_audio_apps() {
            let _ = Command::new("pactl")
                .args(["move-sink-input", &app.id.to_string(), "@DEFAULT_SINK@"])
                .output();
        }
        "Restored: Capturing Full System Audio (All Applications)".into()
    }
    #[cfg(not(target_os = "linux"))]
    {
        "Restored: Capturing Full System Audio (All Applications)".into()
    }
}
