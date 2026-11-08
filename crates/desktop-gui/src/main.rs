#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::app_routing::{
    list_active_audio_apps, route_single_app_to_audioshare, AudioAppEntry,
};
use audioshare_host_engine::discovery_and_os::{
    AdbUsbWatcher, DirectHotspotController, DiscoveryBeacon, DiscoveryScanner,
    VirtualSpeakerAndRadioGuard,
};
use audioshare_host_engine::virtual_mic::{PhoneMicReceiver, PHONE_MIC_PORT};
use audioshare_host_engine::{
    list_valid_host_interfaces, HostConfig, HostEngine, NetworkInterfaceInfo, DEFAULT_PORT,
};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 660.0])
            .with_min_inner_size([680.0, 520.0])
            .with_title("AudioShare Ultra — Zero-Jitter Lossless Audio Router"),
        ..Default::default()
    };

    eframe::run_native(
        "AudioShare Ultra",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(AudioShareApp::new()))
        }),
    )
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum AppTab {
    HostSender,
    ClientReceiver,
}

struct AudioShareApp {
    tab: AppTab,
    interfaces: Vec<NetworkInterfaceInfo>,

    // Host State
    host_engine: Option<HostEngine>,
    beacon: Option<DiscoveryBeacon>,
    adb_watcher: Option<AdbUsbWatcher>,
    os_guard: Option<VirtualSpeakerAndRadioGuard>,
    usb_low_latency_pacing: bool,
    enable_smart_dtx: bool,
    enable_virtual_speaker: bool,
    use_test_tone: bool,
    host_status_msg: String,

    // 1-Hop Wi-Fi Direct SoftAP State
    direct_hotspot_active: bool,
    hotspot_ssid: String,
    hotspot_pass: String,
    hotspot_status: String,

    // Milestone 5 Security, Per-App & Bidirectional Mic State
    enable_pin_encryption: bool,
    pairing_pin: String,
    audio_apps: Vec<AudioAppEntry>,
    selected_app_idx: Option<usize>,
    app_route_status: String,
    phone_mic_receiver: Option<PhoneMicReceiver>,

    // Client State
    scanner: DiscoveryScanner,
    client_engine: Option<ClientEngine>,
    manual_server_ip: String,
    preamp_gain_db: f32,
    usb_buffer_mode: bool,
    drift_history: VecDeque<f32>,
    client_status_msg: String,
}

impl AudioShareApp {
    fn new() -> Self {
        Self {
            tab: AppTab::HostSender,
            interfaces: list_valid_host_interfaces(),
            host_engine: None,
            beacon: None,
            adb_watcher: None,
            os_guard: None,
            usb_low_latency_pacing: false,
            enable_smart_dtx: true,
            enable_virtual_speaker: false,
            use_test_tone: false,
            host_status_msg: "Stopped. Click 'Start Host Server' to broadcast.".into(),
            direct_hotspot_active: false,
            hotspot_ssid: "AudioShare_5G".into(),
            hotspot_pass: "audioshare88".into(),
            hotspot_status: "Router LAN Mode (Standard)".into(),
            enable_pin_encryption: false,
            pairing_pin: "4829".into(),
            audio_apps: list_active_audio_apps(),
            selected_app_idx: None,
            app_route_status: "Capturing Full System Audio".into(),
            phone_mic_receiver: None,
            scanner: DiscoveryScanner::start(),
            client_engine: None,
            manual_server_ip: format!("127.0.0.1:{}", DEFAULT_PORT),
            preamp_gain_db: 0.0,
            usb_buffer_mode: false,
            drift_history: VecDeque::from(vec![0.0; 120]),
            client_status_msg: "Idle. Select a discovered host or enter IP.".into(),
        }
    }

    fn toggle_host(&mut self) {
        if let Some(mut host) = self.host_engine.take() {
            host.stop();
            self.beacon = None;
            self.adb_watcher = None;
            self.os_guard = None;
            self.host_status_msg = "Host stopped.".into();
        } else {
            self.interfaces = list_valid_host_interfaces();
            let cfg = HostConfig {
                bind_port: DEFAULT_PORT,
                subframes_per_packet: if self.usb_low_latency_pacing { 1 } else { 2 },
                use_test_tone: self.use_test_tone,
                enable_smart_dtx: self.enable_smart_dtx,
            };

            // Acquire the virtual speaker / Wi-Fi guard BEFORE the host opens
            // its capture stream, so the PulseAudio monitor source already
            // exists when cpal selects the input device.
            let os_guard =
                VirtualSpeakerAndRadioGuard::acquire(self.enable_virtual_speaker);
            match HostEngine::start(cfg, vec![]) {
                Ok(engine) => {
                    if self.enable_pin_encryption {
                        engine.set_encryption_pin(Some(&self.pairing_pin));
                    }
                    self.os_guard = Some(os_guard);
                    self.beacon = Some(DiscoveryBeacon::start(
                        DEFAULT_PORT,
                        "PC-AudioShare-Ultra".into(),
                    ));
                    self.adb_watcher = Some(AdbUsbWatcher::start(DEFAULT_PORT));
                    self.host_engine = Some(engine);
                    self.host_status_msg =
                        "Broadcasting 48kHz Studio Lossless + [N-1]/[N-2] FEC (802.11e AC_VO)".into();
                }
                Err(e) => {
                    self.host_status_msg = format!("Error: {}", e);
                }
            }
        }
    }

    fn connect_client(&mut self, addr: SocketAddr) {
        if let Some(mut c) = self.client_engine.take() {
            c.stop();
        }
        let cfg = ClientConfig {
            server_addr: addr,
            target_buffer_subframes: if self.usb_buffer_mode { 2.0 } else { 3.5 },
            preamp_gain_db: self.preamp_gain_db,
        };
        let pin_opt = if self.enable_pin_encryption {
            Some(self.pairing_pin.as_str())
        } else {
            None
        };
        match ClientEngine::connect_with_pin(cfg, pin_opt) {
            Ok(engine) => {
                self.client_engine = Some(engine);
                self.client_status_msg = format!("Connected to {} (Zero-Jitter Lock Active)", addr);
            }
            Err(e) => {
                self.client_status_msg = format!("Connection error: {}", e);
            }
        }
    }
}

impl eframe::App for AudioShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("top_nav").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("🎧 AudioShare Ultra");
                ui.separator();
                ui.selectable_value(&mut self.tab, AppTab::HostSender, "📡 Host Server (Send Audio)");
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::ClientReceiver,
                    "🔊 Client Receiver (Receive Audio)",
                );
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                AppTab::HostSender => self.render_host_tab(ui),
                AppTab::ClientReceiver => self.render_client_tab(ui),
            });
        });
    }
}

impl AudioShareApp {
    fn render_host_tab(&mut self, ui: &mut egui::Ui) {
        let is_running = self.host_engine.is_some();

        ui.group(|ui| {
            ui.horizontal(|ui| {
                let btn_text = if is_running {
                    "⏹ Stop Host Server"
                } else {
                    "▶ Start Host Server"
                };
                let btn = egui::Button::new(egui::RichText::new(btn_text).size(16.0).strong());
                if ui.add_sized([200.0, 38.0], btn).clicked() {
                    self.toggle_host();
                }
                ui.label(egui::RichText::new(&self.host_status_msg).color(if is_running {
                    egui::Color32::LIGHT_GREEN
                } else {
                    egui::Color32::GRAY
                }));
            });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.usb_low_latency_pacing,
                    "⚡ 2.5ms USB Pacing (Uncheck for 5.0ms Bundled Wi-Fi Pacing)",
                );
                ui.checkbox(&mut self.enable_smart_dtx, "🔋 Smart DTX Silence Saver");
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.enable_virtual_speaker,
                    "🔇 Auto Virtual Speaker + Win32 60s Scan Blocker",
                );
                ui.checkbox(&mut self.use_test_tone, "🎵 Built-in 440Hz/554Hz Test Chord");
            });
        });

        ui.add_space(8.0);

        // Network Endpoints, 1-Hop Direct SoftAP & ADB Status
        ui.group(|ui| {
            ui.heading("Active Network, USB & 1-Hop Direct Wireless Endpoints");
            if let Some(ref adb) = self.adb_watcher {
                let usb_ok = adb.usb_device_connected.load(Ordering::Relaxed);
                ui.label(if usb_ok {
                    "🔌 ADB USB Device Detected — Zero-Touch Wired Tunnel Active!"
                } else {
                    "🔌 ADB USB Watcher: Waiting for USB cable connection..."
                });
            }
            for iface in &self.interfaces {
                let tag = if iface.is_usb_tethering {
                    "⚡ [USB RNDIS Priority]"
                } else {
                    "📶 [Wi-Fi / Ethernet]"
                };
                ui.monospace(format!("{} {}:{} ({})", tag, iface.ip, DEFAULT_PORT, iface.name));
            }
            ui.separator();
            ui.horizontal(|ui| {
                let hs_btn = if self.direct_hotspot_active {
                    "Stop 1-Hop Direct Hotspot"
                } else {
                    "📡 Start 1-Hop 5GHz Direct Link (Bypass Router)"
                };
                if ui.button(hs_btn).clicked() {
                    if self.direct_hotspot_active {
                        DirectHotspotController::stop_direct_hotspot();
                        self.direct_hotspot_active = false;
                        self.hotspot_status = "Router LAN Mode".into();
                    } else {
                        match DirectHotspotController::start_direct_5ghz_hotspot(
                            &self.hotspot_ssid,
                            &self.hotspot_pass,
                        ) {
                            Ok(msg) => {
                                self.direct_hotspot_active = true;
                                self.hotspot_status = msg;
                            }
                            Err(e) => self.hotspot_status = format!("Hotspot notice: {}", e),
                        }
                    }
                }
                ui.monospace(&self.hotspot_status);
            });
        });

        ui.add_space(8.0);

        // Connected Clients Panel & Bluetooth Video Sync Offset Helper
        ui.group(|ui| {
            ui.heading("Connected Clients & Dynamic Link Governor");
            if let Some(ref host) = self.host_engine {
                let pkts = host.telemetry.packets_sent.load(Ordering::Relaxed);
                let kb = host.telemetry.bytes_sent.load(Ordering::Relaxed) / 1024;
                let dtx = host.telemetry.dtx_silent_mode.load(Ordering::Relaxed);
                let mode_id = host.telemetry.active_codec_mode.load(Ordering::Relaxed);
                let bt_delay = host.telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed);
                let rebinds = host.telemetry.hotplug_rebinds.load(Ordering::Relaxed);

                let mode_str = if dtx {
                    "SMART-DTX SILENCE (0.6 kbps)"
                } else if mode_id == 2 {
                    "GOVERNOR: COMPRESSED LOW-DELAY (Weak Wi-Fi Protection)"
                } else {
                    "GOVERNOR: STUDIO LOSSLESS LPC + [N-1]/[N-2] FEC"
                };

                ui.monospace(format!(
                    "TX Pkts: {} | KB: {} | Hot-Plug Recoveries: {} | Mode: {}",
                    pkts, kb, rebinds, mode_str
                ));

                if bt_delay > 40 {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "🎧 Phone Bluetooth Earbuds Detected (+{} ms A2DP delay). Suggested VLC/Browser Audio Sync Offset: -{} ms",
                            bt_delay,
                            bt_delay + 18
                        ),
                    );
                }

                ui.separator();
                let clients = host.connected_clients();
                if clients.is_empty() {
                    ui.label("Waiting for clients (mDNS/Broadcast Beacon active)...");
                } else {
                    for client in clients {
                        ui.horizontal(|ui| {
                            let badge = if client.is_usb { "[USB]" } else { "[Wi-Fi]" };
                            ui.monospace(format!(
                                "{} {} (Loss: {:.1}% | BT Delay: {}ms)",
                                badge,
                                client.addr,
                                (client.fec_rate_permille as f32) / 10.0,
                                client.bluetooth_delay_ms
                            ));
                            let mute_label = if client.muted { "Unmute" } else { "Mute" };
                            if ui.button(mute_label).clicked() {
                                host.set_client_muted(client.addr, !client.muted);
                            }
                            if ui.button("Kick").clicked() {
                                host.kick_client(client.addr);
                            }
                        });
                    }
                }
            } else {
                ui.label("Start the Host Server to view connected clients and telemetry.");
            }
        });

        ui.add_space(8.0);

        // Milestone 5: PIN Encryption, Per-App Routing & Phone-as-PC-Mic
        ui.group(|ui| {
            ui.heading("🛡️ Security, Per-App Audio Routing & Bidirectional Phone Mic");

            ui.horizontal(|ui| {
                if ui
                    .checkbox(
                        &mut self.enable_pin_encryption,
                        "🔒 ChaCha20 + MAC32 Encryption — PIN:",
                    )
                    .changed()
                {
                    if let Some(ref host) = self.host_engine {
                        let pin = if self.enable_pin_encryption {
                            Some(self.pairing_pin.as_str())
                        } else {
                            None
                        };
                        host.set_encryption_pin(pin);
                    }
                }
                ui.add_sized(
                    [80.0, 22.0],
                    egui::TextEdit::singleline(&mut self.pairing_pin),
                );
                if ui.button("Apply PIN").clicked() {
                    if let Some(ref host) = self.host_engine {
                        let pin = if self.enable_pin_encryption {
                            Some(self.pairing_pin.as_str())
                        } else {
                            None
                        };
                        host.set_encryption_pin(pin);
                    }
                }
            });

            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("🎯 Per-App Capture:");
                if ui.button("🔄 Refresh Apps").clicked() {
                    self.audio_apps = list_active_audio_apps();
                }
                for (idx, app) in self.audio_apps.iter().enumerate() {
                    let selected = self.selected_app_idx == Some(idx);
                    if ui
                        .selectable_label(selected, format!("{} ({})", app.name, app.pid))
                        .clicked()
                    {
                        self.selected_app_idx = Some(idx);
                        match route_single_app_to_audioshare(app) {
                            Ok(msg) => self.app_route_status = msg,
                            Err(err) => self.app_route_status = err,
                        }
                    }
                }
            });
            ui.small("Tip: enable Virtual Speaker before starting the host so capture opens the AudioShare_Ultra monitor.");
            ui.monospace(&self.app_route_status);

            ui.separator();
            ui.horizontal(|ui| {
                let mic_active = self.phone_mic_receiver.is_some();
                let btn_label = if mic_active {
                    "🎙️ Stop Phone-as-PC-Mic"
                } else {
                    "🎙️ Enable Phone-as-PC-Mic (Port 65532)"
                };
                if ui.button(btn_label).clicked() {
                    if let Some(mut m) = self.phone_mic_receiver.take() {
                        m.stop();
                    } else if let Ok(rx) = PhoneMicReceiver::start(PHONE_MIC_PORT) {
                        self.phone_mic_receiver = Some(rx);
                    }
                }

                if let Some(ref mic) = self.phone_mic_receiver {
                    let pkts = mic.mic_packets_received.load(Ordering::Relaxed);
                    let peak = mic.mic_peak_level_pct.load(Ordering::Relaxed);
                    ui.monospace(format!("RX Mic Pkts: {} | Mic Level: {}%", pkts, peak));
                }
            });
        });
    }

    fn render_client_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("1-Tap Auto-Discovered Hosts (LAN & USB)");
            let discovered = self.scanner.discovered_hosts();
            if discovered.is_empty() {
                ui.label("Scanning LAN & USB for AudioShare Ultra beacons...");
            } else {
                for host in discovered {
                    ui.horizontal(|ui| {
                        let badge = if host.is_usb { "⚡ USB" } else { "📶 Wi-Fi" };
                        ui.monospace(format!("{} {} ({})", badge, host.hostname, host.addr));
                        if ui.button("Connect Now").clicked() {
                            self.usb_buffer_mode = host.is_usb;
                            self.connect_client(host.addr);
                        }
                    });
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Manual Host IP:Port:");
                ui.text_edit_singleline(&mut self.manual_server_ip);
                ui.checkbox(&mut self.usb_buffer_mode, "USB 5ms Buffer");
                if ui.button("Connect").clicked() {
                    if let Ok(addr) = self.manual_server_ip.parse::<SocketAddr>() {
                        self.connect_client(addr);
                    } else if let Ok(addr) =
                        format!("{}:{}", self.manual_server_ip, DEFAULT_PORT).parse::<SocketAddr>()
                    {
                        self.connect_client(addr);
                    }
                }
                if self.client_engine.is_some() && ui.button("Disconnect").clicked() {
                    if let Some(mut c) = self.client_engine.take() {
                        c.stop();
                    }
                    self.client_status_msg = "Disconnected.".into();
                }
            });
            ui.label(&self.client_status_msg);
        });

        ui.add_space(10.0);

        ui.group(|ui| {
            ui.heading("Studio Pre-Amp & True-Peak Soft-Knee Limiter (-0.1 dBFS Ceiling)");
            ui.horizontal(|ui| {
                ui.label("Pre-Amp Gain:");
                if ui
                    .add(egui::Slider::new(&mut self.preamp_gain_db, -12.0..=24.0).suffix(" dB"))
                    .changed()
                {
                    if let Some(ref c) = self.client_engine {
                        c.telemetry
                            .preamp_gain_db_tenths
                            .store((self.preamp_gain_db * 10.0) as i32, Ordering::Relaxed);
                    }
                }
                if ui.button("Reset (0 dB)").clicked() {
                    self.preamp_gain_db = 0.0;
                    if let Some(ref c) = self.client_engine {
                        c.telemetry.preamp_gain_db_tenths.store(0, Ordering::Relaxed);
                    }
                }
            });
        });

        ui.add_space(10.0);

        ui.group(|ui| {
            ui.heading("Real-Time WSOLA Sequencer & Clock Drift Telemetry");
            if let Some(ref client) = self.client_engine {
                let pkts = client.telemetry.packets_received.load(Ordering::Relaxed);
                let prim = client.telemetry.primary_frames_played.load(Ordering::Relaxed);
                let fec = client.telemetry.fec_healed_frames.load(Ordering::Relaxed);
                let plc = client.telemetry.plc_extrapolated_frames.load(Ordering::Relaxed);
                let auth_err = client.telemetry.auth_failures.load(Ordering::Relaxed);
                let drift = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                let dtx = client.telemetry.server_dtx_silent.load(Ordering::Relaxed);

                self.drift_history.pop_front();
                self.drift_history.push_back(drift as f32);

                ui.monospace(format!(
                    "RX Packets     : {:<10} | Lossless Subframes : {}",
                    pkts, prim
                ));
                ui.monospace(format!(
                    "FEC Healed     : {:<10} | WSOLA PLC Frames   : {}",
                    fec, plc
                ));
                ui.monospace(format!(
                    "Clock Lock PPM : {:+4} ppm   | Auth Rejections    : {}",
                    drift, auth_err
                ));
                ui.monospace(format!(
                    "Bluetooth KeepAliveState   : {}",
                    if dtx {
                        "ACTIVE (-90 dBFS Dither)"
                    } else {
                        "STREAMING LIVE AUDIO"
                    }
                ));
            } else {
                ui.label("Connect to a Host to view real-time jitter & clock recovery telemetry.");
            }
        });
    }
}
