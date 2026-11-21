use std::collections::HashMap;
#[cfg(target_os = "windows")]
use std::io::BufRead;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::process::Command;
#[cfg(target_os = "windows")]
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::HostTelemetry;

pub const DISCOVERY_PORT: u16 = 65531;
/// Link-local multicast group used alongside subnet broadcast. Some mesh
/// Wi-Fi nodes drop directed/limited broadcasts but still forward multicast.
pub const DISCOVERY_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);
const BEACON_PREFIX: &str = "ASULTRA_BEACON|";
/// Bound the scanner's host map so a spoofed/stale beacon flood cannot grow it
/// without limit. The oldest entry is evicted when a new host pushes past it.
const MAX_DISCOVERED_HOSTS: usize = 64;
/// Refresh cached broadcast targets at most once every 5 s instead of walking
/// the OS interface table on every 250 ms beacon tick.
const BEACON_IFACE_REFRESH: Duration = Duration::from_secs(5);
/// Ports advertised by a discovery beacon. Port 0 is not connectable, so a
/// malformed or truncated beacon must not add a host at `IP:0`.
fn beacon_port(raw: &str) -> u16 {
    raw.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .unwrap_or(crate::DEFAULT_PORT)
}

/// Build a discovery beacon payload. The trailing `enc` field is backward
/// compatible: legacy clients still read the first two fields and ignore it.
fn beacon_payload(host_label: &str, stream_port: u16, encrypted: bool) -> String {
    format!(
        "{}{}|{}|{}",
        BEACON_PREFIX,
        host_label,
        stream_port,
        u8::from(encrypted)
    )
}

/// Parse the `[hostname]|[port]|[enc]` body of a discovery beacon.
fn parse_beacon_fields(rest: &str) -> Option<(&str, u16, bool)> {
    let mut parts = rest.split('|');
    let hostname = parts.next()?;
    let port = beacon_port(parts.next()?);
    let encrypted = parts.next() == Some("1");
    Some((hostname, port, encrypted))
}

/// Docker/WSL/VMware/VirtualBox and Linux bridge interfaces are virtual links,
/// not real LAN segments. Broadcasting discovery beacons on them leaks traffic
/// into virtual subnets and makes phones advertise phantom hosts.
fn is_virtual_interface_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("docker")
        || lower.contains("wsl")
        || lower.contains("vethernet")
        || lower.starts_with("veth")
        || lower.contains("vmware")
        || lower.contains("vmnet")
        || lower.contains("vbox")
        || lower.contains("virtualbox")
        || lower.contains("virbr")
        || lower.contains("br-")
}

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[inline]
pub(crate) fn silent_command(program: &str) -> Command {
    let cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        let mut cmd = cmd;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        cmd
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredHost {
    pub hostname: String,
    pub addr: SocketAddr,
    pub is_usb: bool,
    /// Whether the advertising host requires a PIN (from the `enc` beacon bit).
    pub encrypted: bool,
    pub last_seen: Instant,
}

pub struct DiscoveryBeacon {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DiscoveryBeacon {
    pub fn start(stream_port: u16, host_label: String) -> Self {
        Self::start_with_encryption_flag(stream_port, host_label, Arc::new(AtomicBool::new(false)))
    }

    /// Advertise whether this host has a PIN configured so the phone can hide
    /// the PIN box for open hosts and prompt cleanly for encrypted ones.
    pub fn start_with_encryption_flag(
        stream_port: u16,
        host_label: String,
        encrypted: Arc<AtomicBool>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let run_clone = Arc::clone(&running);

        let handle = std::thread::Builder::new()
            .name("as-ultra-beacon".into())
            .spawn(move || {
                let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
                    return;
                };
                let _ = sock.set_broadcast(true);
                // Link-local scope: never leak discovery beacons past the LAN.
                let _ = sock.set_multicast_ttl_v4(1);
                // Interface enumeration is comparatively expensive. Cache the
                // per-interface (local_ip, broadcast_addr) pairs and refresh only every 5 s.
                let mut iface_targets: Vec<(Ipv4Addr, SocketAddr)> = Vec::new();
                let mut last_iface_scan: Option<Instant> = None;
                while run_clone.load(Ordering::Relaxed) {
                    let payload = beacon_payload(
                        &host_label,
                        stream_port,
                        encrypted.load(Ordering::Relaxed),
                    );
                    let _ = sock.send_to(
                        payload.as_bytes(),
                        SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT)),
                    );
                    let mcast_dest = SocketAddr::from((DISCOVERY_MULTICAST_GROUP, DISCOVERY_PORT));
                    let _ = sock.send_to(payload.as_bytes(), mcast_dest);

                    if last_iface_scan
                        .map(|t| t.elapsed() >= BEACON_IFACE_REFRESH)
                        .unwrap_or(true)
                    {
                        last_iface_scan = Some(Instant::now());
                        iface_targets.clear();
                        // Use the OS-reported broadcast address (correct for
                        // /16, /23, point-to-point and USB tethering links)
                        // instead of assuming /24. Fall back to the legacy
                        // x.y.z.255 form. Virtual NICs (Docker/WSL/VMware/
                        // VBox/virbr/br-) are skipped: they are not real LANs.
                        if let Ok(ifaces) = if_addrs::get_if_addrs() {
                            for iface in ifaces {
                                if iface.is_loopback() || is_virtual_interface_name(&iface.name) {
                                    continue;
                                }
                                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                                    let bcast = v4.broadcast.unwrap_or_else(|| {
                                        let o = v4.ip.octets();
                                        Ipv4Addr::new(o[0], o[1], o[2], 255)
                                    });
                                    iface_targets
                                        .push((v4.ip, SocketAddr::from((bcast, DISCOVERY_PORT))));
                                }
                            }
                        }
                    }
                    let sock_ref = socket2::SockRef::from(&sock);
                    for &(local_ip, bcast_target) in &iface_targets {
                        let _ = sock.send_to(payload.as_bytes(), bcast_target);
                        // Also emit the link-local multicast beacon explicitly out
                        // of each physical NIC so multi-homed hosts (Ethernet +
                        // Wi-Fi / Hotspot / USB RNDIS) discover cleanly even when
                        // the default route points at Ethernet.
                        if sock_ref.set_multicast_if_v4(&local_ip).is_ok() {
                            let _ = sock.send_to(payload.as_bytes(), mcast_dest);
                        }
                    }
                    let _ = sock_ref.set_multicast_if_v4(&Ipv4Addr::UNSPECIFIED);
                    for _ in 0..10 {
                        if !run_clone.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            })
            .ok();

        Self { running, handle }
    }
}

impl Drop for DiscoveryBeacon {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct DiscoveryScanner {
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    hosts: Arc<Mutex<HashMap<SocketAddr, DiscoveredHost>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DiscoveryScanner {
    pub fn start() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let paused = Arc::new(AtomicBool::new(false));
        let hosts = Arc::new(Mutex::new(HashMap::<SocketAddr, DiscoveredHost>::new()));
        let run_clone = Arc::clone(&running);
        let paused_clone = Arc::clone(&paused);
        let hosts_clone = Arc::clone(&hosts);

        let handle = std::thread::Builder::new()
            .name("as-ultra-scanner".into())
            .spawn(move || {
                let Ok(raw_sock) = socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                ) else {
                    return;
                };
                let _ = raw_sock.set_reuse_address(true);
                #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
                let _ = raw_sock.set_reuse_port(true);
                let bind_addr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
                if raw_sock.bind(&bind_addr.into()).is_err() {
                    return;
                }
                let sock: UdpSocket = raw_sock.into();
                // Join the link-local multicast group on the default route and
                // on every valid LAN interface (mesh nodes/APs may only forward
                // multicast on a specific interface).
                let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED);
                for iface in crate::list_valid_host_interfaces() {
                    if let IpAddr::V4(ip) = iface.ip {
                        let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &ip);
                    }
                }
                let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
                let mut buf = [0u8; 256];
                let mut last_mcast_refresh = Instant::now();

                while run_clone.load(Ordering::Relaxed) {
                    // While paused (e.g. the app is in the background or a
                    // network switch is in progress) check FIRST and skip all
                    // work — including the 2 s multicast refresh — sleeping
                    // 250 ms instead of waking every 50 ms. The socket stays
                    // bound; datagrams queue in the kernel and are processed
                    // as soon as scanning resumes.
                    if paused_clone.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    if last_mcast_refresh.elapsed() >= Duration::from_secs(2) {
                        last_mcast_refresh = Instant::now();
                        for iface in crate::list_valid_host_interfaces() {
                            if let IpAddr::V4(ip) = iface.ip {
                                let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &ip);
                            }
                        }
                    }
                    if let Ok((len, src)) = sock.recv_from(&mut buf) {
                        // The thread may have been blocked inside recv_from when
                        // a pause was requested, so re-check before processing.
                        if paused_clone.load(Ordering::Relaxed) {
                            continue;
                        }
                        if let Ok(msg) = std::str::from_utf8(&buf[..len]) {
                            if let Some(rest) = msg.strip_prefix(BEACON_PREFIX) {
                                if let Some((hostname, port, encrypted)) = parse_beacon_fields(rest)
                                {
                                    let host_addr = SocketAddr::new(src.ip(), port);
                                    let is_usb = crate::is_usb_subnet_ip(src.ip());

                                    let mut hosts = hosts_clone.lock().unwrap();
                                    // Bound the map against beacon floods /
                                    // stale entries: evict the least recently
                                    // seen host before adding a new one.
                                    if !hosts.contains_key(&host_addr)
                                        && hosts.len() >= MAX_DISCOVERED_HOSTS
                                    {
                                        if let Some(oldest) = hosts
                                            .iter()
                                            .min_by_key(|(_, v)| v.last_seen)
                                            .map(|(addr, _)| *addr)
                                        {
                                            hosts.remove(&oldest);
                                        }
                                    }
                                    hosts.insert(
                                        host_addr,
                                        DiscoveredHost {
                                            hostname: hostname.to_string(),
                                            addr: host_addr,
                                            is_usb,
                                            encrypted,
                                            last_seen: Instant::now(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    hosts_clone
                        .lock()
                        .unwrap()
                        .retain(|_, v| v.last_seen.elapsed() < Duration::from_secs(4));
                }
            })
            .ok();

        Self {
            running,
            paused,
            hosts,
            handle,
        }
    }

    /// Pause or resume UDP beacon reception. While paused the scanner keeps
    /// its socket bound but does not drain or register any beacons; queued
    /// datagrams are processed after `set_paused(false)`.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn discovered_hosts(&self) -> Vec<DiscoveredHost> {
        let mut list: Vec<DiscoveredHost> = self.hosts.lock().unwrap().values().cloned().collect();
        list.sort_by_key(|h| !h.is_usb);
        list
    }
}

impl Drop for DiscoveryScanner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct AdbUsbWatcher {
    running: Arc<AtomicBool>,
    pub usb_device_connected: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    tcp_bridge_handle: Option<std::thread::JoinHandle<()>>,
}

impl AdbUsbWatcher {
    pub fn start(port: u16) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let usb_device_connected = Arc::new(AtomicBool::new(false));
        let run_clone = Arc::clone(&running);
        let usb_clone = Arc::clone(&usb_device_connected);
        let bridge_run = Arc::clone(&running);
        // Shared across every accepted ADB reverse session so a flood of
        // connects cannot spawn unbounded reader/writer thread pairs.
        let active_sessions = Arc::new(AtomicUsize::new(0));

        let handle = std::thread::Builder::new()
            .name("as-ultra-adb-watcher".into())
            .spawn(move || {
                let mut was_connected = false;
                let mut last_reverse: Option<Instant> = None;
                while run_clone.load(Ordering::Relaxed) {
                    // Enumerate all attached ADB devices in the "device" state so
                    // `adb reverse` still succeeds when multiple phones or an
                    // emulator + phone are connected simultaneously (bare
                    // `adb get-state` / `adb reverse` fails with "more than one
                    // device/emulator").
                    let serials: Vec<String> = silent_command("adb")
                        .args(["devices"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .skip(1)
                                .filter_map(|line| {
                                    let mut parts = line.split_whitespace();
                                    let serial = parts.next()?;
                                    let state = parts.next()?;
                                    if state == "device" && !serial.is_empty() {
                                        Some(serial.to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let has_device = !serials.is_empty();

                    usb_clone.store(has_device, Ordering::Relaxed);

                    // `adb reverse` is not durable across a USB cable yank or
                    // an ADB daemon restart: the tunnel silently disappears
                    // while `adb devices` still reports a device. Re-arm it
                    // on the initial attach (fast path) and then every 10 s
                    // for as long as the device stays attached.
                    if has_device {
                        let initial_attach = !was_connected;
                        let periodic = last_reverse
                            .map(|t| t.elapsed() >= Duration::from_secs(10))
                            .unwrap_or(false);
                        if initial_attach || periodic {
                            let port_spec = format!("tcp:{}", port);
                            for serial in &serials {
                                let _ = silent_command("adb")
                                    .args(["-s", serial, "reverse", &port_spec, &port_spec])
                                    .output();
                            }
                            last_reverse = Some(Instant::now());
                        }
                    } else {
                        last_reverse = None;
                    }
                    was_connected = has_device;
                    // Poll `run_clone` every 50 ms instead of blocking in an
                    // uninterruptible 2 s sleep so `AdbUsbWatcher::drop()`
                    // returns immediately without freezing the desktop GUI.
                    for _ in 0..40 {
                        if !run_clone.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            })
            .ok();

        // Loopback-only TCP_NODELAY bridge for `adb reverse tcp:P tcp:P`.
        // Bind synchronously: the GUI may create this watcher and immediately
        // connect a client, so a listener bound inside the spawned thread
        // would race that first connect. ADB tunnels TCP streams but has no
        // UDP forwarding; this adapts loopback TCP to the host's UDP listener.
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok();
        let tcp_bridge_handle = tcp_listener.and_then(|listener| {
            std::thread::Builder::new()
            .name("as-ultra-adb-tcp-bridge".into())
            .spawn(move || {
                let _ = listener.set_nonblocking(true);
                while bridge_run.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => spawn_host_adb_tcp_session(
                            stream,
                            port,
                            Arc::clone(&bridge_run),
                            Arc::clone(&active_sessions),
                        ),
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
            .ok()
        });

        Self {
            running,
            usb_device_connected,
            handle,
            tcp_bridge_handle,
        }
    }
}

/// Maximum number of simultaneous `adb reverse` TCP sessions bridged by one
/// `AdbUsbWatcher`. Each session owns two detached worker threads, so capping
/// sessions also bounds the thread/channel fan-out from a buggy or hostile peer.
const MAX_ADB_TCP_SESSIONS: usize = 8;

/// Overall deadline for a single framed read (length prefix or packet body).
/// The socket's 200 ms read timeout still unblocks the worker sooner on
/// shutdown; this is only a safety net against a stalled peer.
const ADB_TCP_READ_DEADLINE: Duration = Duration::from_secs(2);

/// One reserved slot in the shared session counter. Cloned into both worker
/// threads so the slot is released only when the last clone drops — i.e. after
/// *both* threads have exited. RAII also covers every early-return / spawn-
/// failure path, so the counter can never leak a slot.
struct AdbSessionSlot(Arc<AtomicUsize>);

impl Drop for AdbSessionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Read a 2-byte big-endian frame length prefix while allowing the connection
/// to remain idle between frames (`filled == 0`) as long as `alive` and
/// `running` hold. Once the first byte of a frame arrives (`filled == 1`),
/// enforces `ADB_TCP_READ_DEADLINE` so a half-open stalled peer still times out.
fn read_frame_len_interruptible<T: Read>(
    reader: &mut T,
    alive: &AtomicBool,
    running: &AtomicBool,
) -> std::io::Result<u16> {
    let mut len_buf = [0u8; 2];
    let mut filled = 0usize;
    let mut partial_deadline: Option<Instant> = None;
    while filled < 2 {
        if !alive.load(Ordering::Relaxed) || !running.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "ADB TCP session stopping",
            ));
        }
        if let Some(dl) = partial_deadline {
            if Instant::now() >= dl {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "ADB TCP length prefix read deadline exceeded",
                ));
            }
        }
        match reader.read(&mut len_buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "ADB TCP peer closed connection",
                ));
            }
            Ok(n) => {
                filled += n;
                if filled == 1 && partial_deadline.is_none() {
                    partial_deadline = Some(Instant::now() + ADB_TCP_READ_DEADLINE);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(u16::from_be_bytes(len_buf))
}

/// Read exactly `buf.len()` bytes, preserving partial progress across
/// `WouldBlock` / `TimedOut` / `Interrupted` instead of restarting
/// at offset 0 and corrupting the wire framing. The deadline is rechecked on
/// every retry so a peer that trickles bytes cannot pin a worker forever.
fn read_exact_interruptible<T: Read>(
    reader: &mut T,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ADB TCP framing read deadline exceeded",
            ));
        }
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "ADB TCP peer closed mid-frame",
                ));
            }
            Ok(n) => filled += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Bridge one `adb reverse` TCP connection to the local UDP `HostEngine`
/// using `[u16_be len][datagram]` framing.
fn spawn_host_adb_tcp_session(
    stream: TcpStream,
    host_udp_port: u16,
    host_running: Arc<AtomicBool>,
    active_sessions: Arc<AtomicUsize>,
) {
    // Reserve a session slot with a CAS loop. At the cap, drop the accepted
    // stream and return without spawning anything.
    let mut observed = active_sessions.load(Ordering::Relaxed);
    loop {
        if observed >= MAX_ADB_TCP_SESSIONS {
            return;
        }
        match active_sessions.compare_exchange_weak(
            observed,
            observed + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
    let slot = Arc::new(AdbSessionSlot(Arc::clone(&active_sessions)));

    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    let Ok(mut tcp_writer) = stream.try_clone() else {
        return;
    };
    // Dedicated clone for the reader thread: whichever thread exits first
    // shuts the socket down and unblocks the other immediately.
    let Ok(tcp_writer_for_reader) = tcp_writer.try_clone() else {
        return;
    };
    let Ok(udp_rx) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) else {
        return;
    };
    if udp_rx.connect((Ipv4Addr::LOCALHOST, host_udp_port)).is_err() {
        return;
    }
    let _ = udp_rx.set_read_timeout(Some(Duration::from_millis(200)));
    let Ok(udp_tx) = udp_rx.try_clone() else {
        return;
    };
    let alive = Arc::new(AtomicBool::new(true));

    let alive_in = Arc::clone(&alive);
    let run_in = Arc::clone(&host_running);
    let slot_in = Arc::clone(&slot);
    let mut tcp_reader = stream;
    let _ = std::thread::Builder::new()
        .name("as-ultra-adb-in".into())
        .spawn(move || {
            let mut pkt = [0u8; crate::MAX_SAFE_UDP_PAYLOAD];
            while alive_in.load(Ordering::Relaxed) && run_in.load(Ordering::Relaxed) {
                let Ok(raw_len) =
                    read_frame_len_interruptible(&mut tcp_reader, &alive_in, &run_in)
                else {
                    break;
                };
                let len = raw_len as usize;
                if len == 0 || len > pkt.len() {
                    break;
                }
                if read_exact_interruptible(
                    &mut tcp_reader,
                    &mut pkt[..len],
                    Instant::now() + ADB_TCP_READ_DEADLINE,
                )
                .is_err()
                {
                    break;
                }
                let _ = udp_tx.send(&pkt[..len]);
            }
            alive_in.store(false, Ordering::Release);
            let _ = tcp_writer_for_reader.shutdown(std::net::Shutdown::Both);
            drop(slot_in);
        });

    let alive_out = Arc::clone(&alive);
    let run_out = Arc::clone(&host_running);
    let slot_out = Arc::clone(&slot);
    let _ = std::thread::Builder::new()
        .name("as-ultra-adb-out".into())
        .spawn(move || {
            let mut framed = [0u8; 2 + crate::MAX_SAFE_UDP_PAYLOAD];
            while alive_out.load(Ordering::Relaxed) && run_out.load(Ordering::Relaxed) {
                match udp_rx.recv(&mut framed[2..]) {
                    Ok(len) if len > 0 && len <= crate::MAX_SAFE_UDP_PAYLOAD => {
                        framed[0..2].copy_from_slice(&(len as u16).to_be_bytes());
                        if tcp_writer.write_all(&framed[..2 + len]).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
            alive_out.store(false, Ordering::Release);
            let _ = tcp_writer.shutdown(std::net::Shutdown::Both);
            drop(slot_out);
        });
}

impl Drop for AdbUsbWatcher {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.tcp_bridge_handle.take() {
            let _ = h.join();
        }
    }
}

/// 1-Hop Wi-Fi Direct / SoftAP Hotspot Controller (Bypasses congested or AP-isolated routers)
pub struct DirectHotspotController;

#[cfg(target_os = "windows")]
fn windows_hotspot_holder() -> &'static Mutex<Option<Child>> {
    static HOLDER: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    HOLDER.get_or_init(|| Mutex::new(None))
}

impl DirectHotspotController {
    pub fn start_direct_5ghz_hotspot(ssid: &str, password: &str) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("nmcli")
                .args([
                    "device", "wifi", "hotspot", "ssid", ssid, "password", password, "band", "a",
                ])
                .output()
                .map_err(|e| format!("Failed to invoke nmcli: {}", e))?;

            if output.status.success() {
                Ok(format!(
                    "1-Hop 5GHz Direct Link '{}' Active (IP: 10.42.0.1)",
                    ssid
                ))
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
        }

        #[cfg(target_os = "windows")]
        {
            Self::stop_direct_hotspot();
            let safe_ssid = ssid.replace('\'', "''");
            let safe_pass = password.replace('\'', "''");
            // Modern Windows 10/11 Wi-Fi Direct Legacy SoftAP (works without
            // an internet connection and on WDI Wi-Fi 6/6E/7 drivers where
            // `netsh wlan start hostednetwork` was removed). The SoftAP stays
            // active for the lifetime of this hidden holder process.
            let script = format!(
                "$ErrorActionPreference = 'Stop'; \
                 $pub = [Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher,Windows.Devices.WiFiDirect,ContentType=WindowsRuntime]::new(); \
                 $pub.Advertisement.ListenStateDiscoverability = [Windows.Devices.WiFiDirect.WiFiDirectAdvertisementListenStateDiscoverability]::Intensive; \
                 $pub.Advertisement.IsAutonomousGroupOwnerEnabled = $true; \
                 $legacy = $pub.Advertisement.LegacySettings; \
                 $legacy.IsEnabled = $true; \
                 $legacy.Ssid = '{}'; \
                 $cred = [Windows.Security.Credentials.PasswordCredential,Windows.Security.Credentials,ContentType=WindowsRuntime]::new(); \
                 $cred.Password = '{}'; \
                 $legacy.Passphrase = $cred; \
                 $pub.Start(); \
                 for ($i = 0; $i -lt 40; $i++) {{ \
                     $st = $pub.Status.ToString(); \
                     if ($st -eq 'Started') {{ Write-Output 'ASULTRA_SOFTAP_READY'; break }} \
                     if ($st -eq 'Aborted' -or $st -eq 'Stopped') {{ Write-Output \"ASULTRA_SOFTAP_FAILED:$st\"; exit 1 }} \
                     Start-Sleep -Milliseconds 100 \
                 }}; \
                 if ($pub.Status.ToString() -ne 'Started') {{ Write-Output 'ASULTRA_SOFTAP_FAILED:Timeout'; exit 1 }}; \
                 while ($true) {{ Start-Sleep -Seconds 3600 }}",
                safe_ssid, safe_pass
            );
            let mut child = silent_command("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Failed to launch WinRT SoftAP host: {}", e))?;
            // `$pub.Start()` is asynchronous. Returning before the SoftAP is
            // actually up makes the phone's first connection fail, so wait
            // for the sentinel emitted by the script (or kill + fail).
            let stdout = child.stdout.take();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
            let _ = std::thread::Builder::new()
                .name("as-ultra-softap-ready".into())
                .spawn(move || {
                    if let Some(pipe) = stdout {
                        let mut reader = std::io::BufReader::new(pipe);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    if line.contains("ASULTRA_SOFTAP_READY") {
                                        let _ = ready_tx.send(true);
                                        return;
                                    }
                                    if line.contains("ASULTRA_SOFTAP_FAILED") {
                                        let _ = ready_tx.send(false);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    let _ = ready_tx.send(false);
                });
            match ready_rx.recv_timeout(Duration::from_secs(8)) {
                Ok(true) => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Wi-Fi Direct SoftAP '{}' could not start (check Wi-Fi adapter is enabled)",
                        ssid
                    ));
                }
            }
            let mut holder = windows_hotspot_holder().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut prev) = holder.take() {
                let _ = prev.kill();
                let _ = prev.wait();
            }
            *holder = Some(child);
            Ok(format!(
                "1-Hop Direct Link '{}' Active (IP: 192.168.137.1)",
                ssid
            ))
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = (ssid, password);
            Err("Unsupported OS for Direct Hotspot".into())
        }
    }

    pub fn stop_direct_hotspot() {
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("nmcli")
                .args(["connection", "down", "Hotspot"])
                .output();
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(mut child) = windows_hotspot_holder().lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = silent_command("netsh")
                .args(["wlan", "stop", "hostednetwork"])
                .output();
        }
    }
}

/// Pure-Rust RFC 4648 Base64 encoder for PowerShell `-EncodedCommand` (UTF-16LE),
/// avoiding nested-quote escaping failures on paths containing spaces.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn encode_powershell_utf16le_base64(script: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

/// Returns `true` if Windows Defender Firewall already has the inbound
/// `AudioShare_Ultra_UDP` allow rule (does not require Administrator rights).
/// Always returns `true` on non-Windows platforms.
pub fn is_windows_firewall_configured() -> bool {
    #[cfg(target_os = "windows")]
    {
        silent_command("netsh")
            .args(["advfirewall", "firewall", "show", "rule", "name=AudioShare_Ultra_UDP"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// Opens UDP ports 65530..65532 in Linux (ufw / firewalld) or Windows Defender Firewall
pub fn open_os_firewall_ports() -> String {
    #[cfg(target_os = "linux")]
    {
        let cmd = "ufw allow 65530:65532/udp || (firewall-cmd --add-port=65530-65532/udp && firewall-cmd --runtime-to-permanent)";
        match Command::new("pkexec").args(["sh", "-c", cmd]).status() {
            Ok(s) if s.success() => "Firewall ports 65530-65532/UDP opened successfully!".into(),
            _ => "Firewall rule skipped or no active firewall detected.".into(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // PowerShell single-quoted strings only treat '' as an escaped quote;
        // `$`, backticks and double quotes stay literal. Doubling `'` keeps a
        // path with quotes from terminating the literal and expanding code.
        let exe_path = std::env::current_exe()
            .map(|p| p.to_string_lossy().replace('\'', "''"))
            .unwrap_or_default();
        // Add BOTH a port rule AND a program allow rule across `profile=any`
        // (Public/Hotspot + Private + Domain), deleting any duplicate rules
        // first. Uses `Start-Process -Verb RunAs` to trigger a standard Windows
        // UAC prompt instead of failing silently as a standard user. The inner
        // script is passed as UTF-16LE Base64 via `-EncodedCommand`, so paths
        // with spaces or Unicode need no nested-quote escaping.
        let mut script = String::from(
            "netsh advfirewall firewall delete rule name=\"AudioShare_Ultra_UDP\" | Out-Null\n\
             netsh advfirewall firewall delete rule name=\"AudioShare_Ultra_App\" | Out-Null\n\
             netsh advfirewall firewall add rule name=\"AudioShare_Ultra_UDP\" dir=in action=allow protocol=UDP localport=65530-65532 profile=any | Out-Null\n",
        );
        if !exe_path.is_empty() {
            // `$exe` is assigned with a single-quoted literal (see above) and
            // expanded by PowerShell into the quoted netsh `program=` value.
            script.push_str(&format!(
                "$exe = '{exe_path}'; netsh advfirewall firewall add rule name=\"AudioShare_Ultra_App\" dir=in action=allow program=\"$exe\" profile=any | Out-Null\n"
            ));
        }
        let encoded = encode_powershell_utf16le_base64(&script);
        let elevated = silent_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Start-Process -FilePath 'powershell' -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{encoded}'"
                ),
            ])
            .status();
        if elevated.map(|s| s.success()).unwrap_or(false) && is_windows_firewall_configured() {
            "Windows Firewall opened (Private + Public/Hotspot UDP 65530-65532)!".into()
        } else {
            "UAC declined or blocked — allow UDP 65530-65532 (Private + Public) in Windows Firewall.".into()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        "Unsupported OS".into()
    }
}

/// Configures or removes OS Boot Auto-Start (Solves Issue #170 on Linux XDG & Windows Registry)
pub fn set_os_boot_autostart(enable: bool) -> String {
    let Ok(exe_path) = std::env::current_exe() else {
        return "Failed to resolve executable path".into();
    };
    let exe_str = exe_path.to_string_lossy();

    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let autostart_dir = std::path::Path::new(&home).join(".config/autostart");
        let desktop_file = autostart_dir.join("audioshare-ultra.desktop");
        if enable {
            let _ = std::fs::create_dir_all(&autostart_dir);
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=AudioShare Ultra\nExec=\"{}\" --autostart\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe_str
            );
            match std::fs::write(&desktop_file, content) {
                Ok(_) => "Enabled Linux Boot Auto-Start (~/.config/autostart)".into(),
                Err(e) => format!("Auto-start error: {}", e),
            }
        } else {
            let _ = std::fs::remove_file(&desktop_file);
            "Disabled Linux Boot Auto-Start".into()
        }
    }

    #[cfg(target_os = "windows")]
    {
        if enable {
            match silent_command("reg")
                .args([
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "AudioShareUltra",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &format!("\"{}\" --autostart", exe_str),
                    "/f",
                ])
                .output()
            {
                Ok(o) if o.status.success() => "Enabled Windows Boot Auto-Start (HKCU Run)".into(),
                Ok(o) => format!(
                    "Failed to enable Boot Auto-Start: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => format!("Failed to enable Boot Auto-Start: {e}"),
            }
        } else {
            let _ = silent_command("reg")
                .args([
                    "delete",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "AudioShareUltra",
                    "/f",
                ])
                .output();
            "Disabled Windows Boot Auto-Start".into()
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (enable, &exe_str);
        "Unsupported OS for boot auto-start".into()
    }
}

/// Polls the OS master output volume/mute on a dedicated thread and mirrors it
/// into `HostTelemetry::host_volume`. This lets physical PC keyboard media
/// keys propagate to connected phones without ever invoking `pactl` from the
/// real-time audio TX loop.
pub struct HostVolumeMonitor {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HostVolumeMonitor {
    pub fn start(telemetry: Arc<HostTelemetry>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let run_clone = Arc::clone(&running);

        let handle = std::thread::Builder::new()
            .name("as-ultra-volume-monitor".into())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                {
                    let mut last_os_volume: Option<u8> = None;
                    while run_clone.load(Ordering::Relaxed) {
                        if let Some(volume) = read_linux_master_volume() {
                            // Only overwrite the telemetry when the OS value
                            // itself changed. A GUI slider adjustment therefore
                            // persists until the user actually presses a system
                            // volume/mute key.
                            if last_os_volume != Some(volume) {
                                telemetry.host_volume.store(volume, Ordering::Relaxed);
                                last_os_volume = Some(volume);
                            }
                        }
                        for _ in 0..10 {
                            if !run_clone.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    // On Windows, keep `host_volume` at full 100% digital unity
                    // (0 dBFS) unless the user adjusts the AudioShare Ultra
                    // Master Volume slider. Mirroring Windows physical speaker
                    // endpoint volume halved/muted phone audio whenever the user
                    // turned down laptop speakers to avoid echo, and doubled
                    // attenuation on APO endpoints where WASAPI loopback already
                    // reflects software volume.
                    let _ = telemetry;
                    let _ = run_clone;
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    let _ = telemetry;
                    while run_clone.load(Ordering::Relaxed) {
                        for _ in 0..10 {
                            if !run_clone.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            })
            .ok();

        Self { running, handle }
    }
}

impl Drop for HostVolumeMonitor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Parse the first channel percentage from `pactl get-sink-volume` output.
#[cfg(target_os = "linux")]
fn parse_pactl_volume_percent(output: &str) -> Option<u8> {
    let pct_end = output.find('%')?;
    let number = output[..pct_end].split_whitespace().last()?;
    number.parse::<u32>().ok().map(|v| v.min(100) as u8)
}

#[cfg(target_os = "linux")]
fn read_linux_master_volume() -> Option<u8> {
    let mute = Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !mute.status.success() {
        return None;
    }
    let muted = String::from_utf8_lossy(&mute.stdout)
        .to_ascii_lowercase()
        .contains("yes");
    if muted {
        return Some(0);
    }

    let volume = Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !volume.status.success() {
        return None;
    }
    parse_pactl_volume_percent(&String::from_utf8_lossy(&volume.stdout))
}

/// Zero-dependency Windows CoreAudio COM monitor for master speaker volume.
///
/// COM initialization and the `IMMDeviceEnumerator` are created once and kept
/// for the whole monitor thread lifetime. Rebuilding them on every 500 ms poll
/// repeatedly tore down COM apartment state and cost far more than the read.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn run_windows_volume_monitor(telemetry: &HostTelemetry, running: &AtomicBool) {
    use core::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IMMDeviceEnumerator) -> u32,
        enum_audio_endpoints: usize,
        get_default_audio_endpoint: unsafe extern "system" fn(
            *mut IMMDeviceEnumerator,
            u32,
            u32,
            *mut *mut IMMDevice,
        ) -> i32,
    }
    #[repr(C)]
    struct IMMDeviceEnumerator {
        lp_vtbl: *const IMMDeviceEnumeratorVtbl,
    }

    #[repr(C)]
    struct IMMDeviceVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IMMDevice) -> u32,
        activate: unsafe extern "system" fn(
            *mut IMMDevice,
            *const Guid,
            u32,
            *const c_void,
            *mut *mut IAudioEndpointVolume,
        ) -> i32,
    }
    #[repr(C)]
    struct IMMDevice {
        lp_vtbl: *const IMMDeviceVtbl,
    }

    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IAudioEndpointVolume) -> u32,
        register_control_change_notify: usize,
        unregister_control_change_notify: usize,
        get_channel_count: usize,
        set_master_volume_level: usize,
        set_master_volume_level_scalar: usize,
        get_master_volume_level: usize,
        get_master_volume_level_scalar:
            unsafe extern "system" fn(*mut IAudioEndpointVolume, *mut f32) -> i32,
        set_channel_volume_level: usize,
        set_channel_volume_level_scalar: usize,
        get_channel_volume_level: usize,
        get_channel_volume_level_scalar: usize,
        set_mute: usize,
        get_mute: unsafe extern "system" fn(*mut IAudioEndpointVolume, *mut i32) -> i32,
    }
    #[repr(C)]
    struct IAudioEndpointVolume {
        lp_vtbl: *const IAudioEndpointVolumeVtbl,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> i32;
        fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }

    // Balance every successful CoInitializeEx, including S_FALSE when COM
    // was already initialized on this thread, on all early-return paths.
    struct ComInit(bool);
    impl ComInit {
        fn new() -> Self {
            Self(unsafe { CoInitializeEx(core::ptr::null_mut(), 0) >= 0 })
        }
    }
    impl Drop for ComInit {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xBCDE_0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xA956_64D2,
        data2: 0x9614,
        data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IAUDIO_ENDPOINT_VOLUME: Guid = Guid {
        data1: 0x5CDF_2C82,
        data2: 0x841E,
        data3: 0x4546,
        data4: [0x97, 0x22, 0x0C, 0xF7, 0x40, 0x78, 0x22, 0x9A],
    };

    unsafe {
        let _com_init = ComInit::new();
        let mut enumerator: *mut IMMDeviceEnumerator = core::ptr::null_mut();
        if CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            core::ptr::null_mut(),
            1, // CLSCTX_INPROC_SERVER
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator as *mut _ as *mut *mut c_void,
        ) < 0
            || enumerator.is_null()
        {
            return;
        }
        let mut last_os_volume: Option<u8> = None;
        while running.load(Ordering::Relaxed) {
            let mut device: *mut IMMDevice = core::ptr::null_mut();
            let hr_dev = ((*(*enumerator).lp_vtbl).get_default_audio_endpoint)(
                enumerator,
                0, // eRender
                0, // eConsole
                &mut device,
            );
            if hr_dev >= 0 && !device.is_null() {
                let mut ep_vol: *mut IAudioEndpointVolume = core::ptr::null_mut();
                let hr_act = ((*(*device).lp_vtbl).activate)(
                    device,
                    &IID_IAUDIO_ENDPOINT_VOLUME,
                    1,
                    core::ptr::null(),
                    &mut ep_vol,
                );
                ((*(*device).lp_vtbl).release)(device);
                if hr_act >= 0 && !ep_vol.is_null() {
                    let mut muted: i32 = 0;
                    let mut scalar: f32 = 1.0;
                    let ok_mute = ((*(*ep_vol).lp_vtbl).get_mute)(ep_vol, &mut muted) >= 0;
                    let ok_vol =
                        ((*(*ep_vol).lp_vtbl).get_master_volume_level_scalar)(ep_vol, &mut scalar)
                            >= 0;
                    ((*(*ep_vol).lp_vtbl).release)(ep_vol);
                    let volume = if ok_mute && muted != 0 {
                        Some(0u8)
                    } else if ok_vol {
                        Some((scalar.clamp(0.0, 1.0) * 100.0).round() as u8)
                    } else {
                        None
                    };
                    if let Some(volume) = volume {
                        if last_os_volume != Some(volume) {
                            telemetry.host_volume.store(volume, Ordering::Relaxed);
                            last_os_volume = Some(volume);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        ((*(*enumerator).lp_vtbl).release)(enumerator);
    }
}

/// Virtual Speaker & OS Wi-Fi Scan Suppressor Guard.
///
/// `acquire()` loads/registers the virtual endpoint but leaves the user's
/// current routing untouched. The desktop GUI calls `activate()` once at
/// least one phone is connected and `deactivate()` when the last phone leaves,
/// so the physical speakers are only replaced while audio is being streamed.
pub struct VirtualSpeakerAndRadioGuard {
    /// The virtual endpoint is available and may be activated.
    enabled: bool,
    /// The virtual endpoint is currently the default routing target.
    active: bool,
    #[cfg(target_os = "linux")]
    pactl_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_sink: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_source: Option<String>,
    #[cfg(target_os = "windows")]
    prev_default_sink_id: Option<Vec<u16>>,
    #[cfg(target_os = "windows")]
    virtual_sink_id: Option<Vec<u16>>,
}

#[cfg(target_os = "linux")]
fn pactl_get_default(kind: &str) -> Option<String> {
    let arg = format!("get-default-{}", kind);
    let output = Command::new("pactl").arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(target_os = "linux")]
fn pactl_set_default(kind: &str, value: &str) {
    let arg = format!("set-default-{}", kind);
    let _ = Command::new("pactl").args([arg.as_str(), value]).output();
}

/// True when a PulseAudio sink with this exact name is already registered.
#[cfg(target_os = "linux")]
fn pactl_sink_exists(name: &str) -> bool {
    let Ok(output) = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(name))
}

/// Linux names wireless NICs `wl*` (e.g. `wlan0`, `wlp2s0`, `wlo1`).
#[cfg(target_os = "linux")]
fn is_wireless_interface_name(name: &str) -> bool {
    name.starts_with("wl")
}

/// Enumerate every wireless interface from sysfs, including interfaces that
/// are currently down and therefore absent from `list_valid_host_interfaces`.
#[cfg(target_os = "linux")]
fn wireless_interface_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_wireless_interface_name(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    if names.is_empty() {
        // Fallback for containers/chroots without sysfs.
        names.push("wlan0".into());
    }
    names
}

#[cfg(target_os = "linux")]
fn disable_wifi_power_save() {
    for iface in wireless_interface_names() {
        let _ = Command::new("iw")
            .args(["dev", &iface, "set", "power_save", "off"])
            .output();
    }
}

impl VirtualSpeakerAndRadioGuard {
    /// Register the virtual endpoint (if enabled) without changing the user's
    /// current default sink/source. Call `activate()` when a client connects.
    pub fn acquire(enable_virtual_speaker: bool) -> Self {
        #[cfg(target_os = "linux")]
        {
            let prev_default_sink =
                pactl_get_default("sink").filter(|s| s != "AudioShare_Ultra");
            let prev_default_source =
                pactl_get_default("source").filter(|s| s != "AudioShare_Ultra.monitor");
            let mut pactl_module_id = None;
            let sink_ready = if enable_virtual_speaker {
                if pactl_sink_exists("AudioShare_Ultra") {
                    // A previous session (or another guard) already registered
                    // this sink. Reuse it, but never unload a module we did
                    // not create in Drop.
                    true
                } else {
                    if let Ok(output) = Command::new("pactl")
                        .args([
                            "load-module",
                            "module-null-sink",
                            "sink_name=AudioShare_Ultra",
                            "sink_properties=device.description=AudioShare_Wireless_Speaker",
                        ])
                        .output()
                    {
                        if output.status.success() {
                            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            if !id.is_empty() {
                                pactl_module_id = Some(id);
                            }
                        }
                    }
                    pactl_module_id.is_some()
                }
            } else {
                false
            };
            disable_wifi_power_save();

            Self {
                enabled: sink_ready,
                active: false,
                pactl_module_id,
                prev_default_sink,
                prev_default_source,
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Suppress Windows 60-second background Wi-Fi roaming/location channel scans
            set_windows_wlan_media_streaming_mode(true);
            let virtual_sink_id = if enable_virtual_speaker {
                crate::app_routing::windows_endpoint_switch::find_virtual_playback_id()
            } else {
                None
            };
            let prev_default_sink_id = if virtual_sink_id.is_some() {
                crate::app_routing::windows_endpoint_switch::default_playback_id()
                    .filter(|id| Some(id) != virtual_sink_id.as_ref())
            } else {
                None
            };
            Self {
                enabled: virtual_sink_id.is_some(),
                active: false,
                prev_default_sink_id,
                virtual_sink_id,
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = enable_virtual_speaker;
            Self {
                enabled: false,
                active: false,
            }
        }
    }

    /// Returns `true` if a virtual playback endpoint was found/created and can
    /// be switched to while clients are connected.
    pub fn is_virtual_speaker_available(&self) -> bool {
        self.enabled
    }

    /// Route the default output into the virtual speaker and capture its
    /// monitor. Idempotent; safe to call from the GUI update loop.
    pub fn activate(&mut self) {
        if !self.enabled || self.active {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            pactl_set_default("sink", "AudioShare_Ultra");
            pactl_set_default("source", "AudioShare_Ultra.monitor");
            self.active = true;
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(ref id) = self.virtual_sink_id {
                crate::app_routing::windows_endpoint_switch::set_default_endpoint(id);
            }
            self.active = true;
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            self.active = true;
        }
    }

    /// Restore the physical default sink/source. Idempotent.
    pub fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(ref sink) = self.prev_default_sink {
                pactl_set_default("sink", sink);
            }
            if let Some(ref source) = self.prev_default_source {
                pactl_set_default("source", source);
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(ref id) = self.prev_default_sink_id {
                crate::app_routing::windows_endpoint_switch::set_default_endpoint(id);
            }
        }
        self.active = false;
    }
}

impl Drop for VirtualSpeakerAndRadioGuard {
    fn drop(&mut self) {
        self.deactivate();

        #[cfg(target_os = "linux")]
        if let Some(ref id) = self.pactl_module_id {
            let _ = Command::new("pactl").args(["unload-module", id]).output();
        }

        #[cfg(target_os = "windows")]
        {
            set_windows_wlan_media_streaming_mode(false);
        }
    }
}

/// Dynamically invokes Win32 `wlanapi.dll` `WlanSetInterface(..., wlan_intf_opcode_media_streaming_mode)`
/// to prevent Windows from scanning other Wi-Fi channels every 60 seconds during real-time audio streaming.
#[cfg(target_os = "windows")]
fn set_windows_wlan_media_streaming_mode(enable: bool) {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct WlanInterfaceInfo {
        interface_guid: Guid,
        interface_description: [u16; 256],
        is_state: u32,
    }

    #[repr(C)]
    struct WlanInterfaceInfoList {
        number_of_items: u32,
        index: u32,
        interface_info: [WlanInterfaceInfo; 1],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut core::ffi::c_void;
        fn GetProcAddress(
            module: *mut core::ffi::c_void,
            name: *const u8,
        ) -> *mut core::ffi::c_void;
        fn FreeLibrary(module: *mut core::ffi::c_void) -> i32;
    }

    type FnWlanOpenHandle = unsafe extern "system" fn(
        u32,
        *mut core::ffi::c_void,
        *mut u32,
        *mut *mut core::ffi::c_void,
    ) -> u32;
    type FnWlanEnumInterfaces = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut WlanInterfaceInfoList,
    ) -> u32;
    type FnWlanSetInterface = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *const Guid,
        u32,
        u32,
        *const core::ffi::c_void,
        *mut core::ffi::c_void,
    ) -> u32;
    type FnWlanFreeMemory = unsafe extern "system" fn(*mut core::ffi::c_void);
    type FnWlanCloseHandle =
        unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> u32;

    unsafe {
        let hmod = LoadLibraryA(b"wlanapi.dll\0".as_ptr());
        if hmod.is_null() {
            return;
        }

        let p_open = GetProcAddress(hmod, b"WlanOpenHandle\0".as_ptr());
        let p_enum = GetProcAddress(hmod, b"WlanEnumInterfaces\0".as_ptr());
        let p_set = GetProcAddress(hmod, b"WlanSetInterface\0".as_ptr());
        let p_free = GetProcAddress(hmod, b"WlanFreeMemory\0".as_ptr());
        let p_close = GetProcAddress(hmod, b"WlanCloseHandle\0".as_ptr());

        if !p_open.is_null()
            && !p_enum.is_null()
            && !p_set.is_null()
            && !p_free.is_null()
            && !p_close.is_null()
        {
            let wlan_open: FnWlanOpenHandle = core::mem::transmute(p_open);
            let wlan_enum: FnWlanEnumInterfaces = core::mem::transmute(p_enum);
            let wlan_set: FnWlanSetInterface = core::mem::transmute(p_set);
            let wlan_free: FnWlanFreeMemory = core::mem::transmute(p_free);
            let wlan_close: FnWlanCloseHandle = core::mem::transmute(p_close);

            let mut negotiated_version = 0u32;
            let mut client_handle: *mut core::ffi::c_void = core::ptr::null_mut();

            if wlan_open(
                2,
                core::ptr::null_mut(),
                &mut negotiated_version,
                &mut client_handle,
            ) == 0
            {
                let mut list_ptr: *mut WlanInterfaceInfoList = core::ptr::null_mut();
                if wlan_enum(client_handle, core::ptr::null_mut(), &mut list_ptr) == 0
                    && !list_ptr.is_null()
                {
                    let count = (*list_ptr).number_of_items as usize;
                    let first_ptr = (*list_ptr).interface_info.as_ptr();
                    let val_bool: i32 = if enable { 1 } else { 0 };
                    // wlan_intf_opcode_media_streaming_mode = 3
                    const OPCODE_MEDIA_STREAMING_MODE: u32 = 3;

                    for i in 0..count {
                        let info = &*first_ptr.add(i);
                        let _ = wlan_set(
                            client_handle,
                            &info.interface_guid,
                            OPCODE_MEDIA_STREAMING_MODE,
                            core::mem::size_of::<i32>() as u32,
                            &val_bool as *const _ as *const core::ffi::c_void,
                            core::ptr::null_mut(),
                        );
                    }
                    wlan_free(list_ptr as *mut core::ffi::c_void);
                }
                let _ = wlan_close(client_handle, core::ptr::null_mut());
            }
        }
        FreeLibrary(hmod);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_powershell_utf16le_base64() {
        // Acceptance test (n1 item 2): "hi" -> UTF-16LE `68 00 69 00` -> base64.
        assert_eq!(encode_powershell_utf16le_base64("hi"), "aABpAA==");
        // Non-ASCII: "é" (U+00E9) -> UTF-16LE `E9 00` -> base64.
        assert_eq!(encode_powershell_utf16le_base64("é"), "6QA=");
    }

    #[test]
    fn test_virtual_interface_names_are_filtered() {
        for name in [
            "docker0",
            "veth1234",
            "vEthernet (WSL)",
            "vmnet1",
            "VMware Network Adapter VMnet8",
            "VirtualBox Host-Only Network",
            "virbr0",
            "br-abc123",
        ] {
            assert!(is_virtual_interface_name(name), "expected virtual: {name}");
        }
        for name in ["eth0", "wlan0", "enp3s0", "Wi-Fi", "lo"] {
            assert!(
                !is_virtual_interface_name(name),
                "expected physical: {name}"
            );
        }
    }

    #[test]
    fn test_discovery_beacon_rejects_port_zero() {
        assert_eq!(beacon_port("65530"), 65530);
        assert_eq!(beacon_port("0"), crate::DEFAULT_PORT);
        assert_eq!(beacon_port("garbage"), crate::DEFAULT_PORT);
        assert_eq!(beacon_port("65536"), crate::DEFAULT_PORT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_pactl_volume_parser_handles_typical_output() {
        let typical = "Volume: front-left: 65536 / 100% / 0.00 dB,   \
                       front-right: 65536 / 100% / 0.00 dB\n        balance 0.00\n";
        assert_eq!(parse_pactl_volume_percent(typical), Some(100));

        let quiet = "Volume: front-left: 26214 /  40% / -23.88 dB,   \
                     front-right: 26214 /  40% / -23.88 dB\n";
        assert_eq!(parse_pactl_volume_percent(quiet), Some(40));
        assert_eq!(parse_pactl_volume_percent("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_wireless_interface_name_detection() {
        assert!(is_wireless_interface_name("wlan0"));
        assert!(is_wireless_interface_name("wlp2s0"));
        assert!(is_wireless_interface_name("wlo1"));
        assert!(!is_wireless_interface_name("eth0"));
        assert!(!is_wireless_interface_name("enx001122334455"));
        assert!(!is_wireless_interface_name("lo"));
    }

    #[test]
    fn test_multicast_discovery_group_is_link_local() {
        assert!(DISCOVERY_MULTICAST_GROUP.is_multicast());
        assert!(DISCOVERY_MULTICAST_GROUP.octets()[0] >= 224);
        assert!(DISCOVERY_MULTICAST_GROUP.octets()[0] <= 239);
    }

    #[test]
    fn test_beacon_encryption_flag_roundtrip_and_legacy_compat() {
        let payload = beacon_payload("Studio-PC", 65530, true);
        assert_eq!(payload, "ASULTRA_BEACON|Studio-PC|65530|1");
        let rest = payload.strip_prefix(BEACON_PREFIX).unwrap();
        assert_eq!(parse_beacon_fields(rest), Some(("Studio-PC", 65530, true)));

        // Legacy two-field payloads still parse and default to open.
        let legacy = "Studio-PC|65530";
        assert_eq!(parse_beacon_fields(legacy), Some(("Studio-PC", 65530, false)));
        let open = beacon_payload("Open-PC", 65530, false);
        assert_eq!(
            parse_beacon_fields(open.strip_prefix(BEACON_PREFIX).unwrap()),
            Some(("Open-PC", 65530, false))
        );
    }

    #[test]
    fn test_beacon_enc_flag_zero_vs_one_roundtrip() {
        // A `|1` beacon must advertise encryption; a `|0` beacon must not.
        let enc = format!("{}{}|{}|1", BEACON_PREFIX, "Encrypted-Host", 41001);
        let open = format!("{}{}|{}|0", BEACON_PREFIX, "Open-Host", 41002);
        assert_eq!(
            parse_beacon_fields(enc.strip_prefix(BEACON_PREFIX).unwrap()),
            Some(("Encrypted-Host", 41001, true))
        );
        assert_eq!(
            parse_beacon_fields(open.strip_prefix(BEACON_PREFIX).unwrap()),
            Some(("Open-Host", 41002, false))
        );

        // The exact strings a DiscoveryScanner feeds through parse_beacon_fields.
        let generated_enc = beacon_payload("Encrypted-Host", 41001, true);
        let generated_open = beacon_payload("Open-Host", 41002, false);
        assert_eq!(generated_enc, enc);
        assert_eq!(generated_open, open);
    }

    #[test]
    fn test_discovery_scanner_pause_resume() {
        let scanner = DiscoveryScanner::start();
        // Let the scanner thread bind the discovery socket before we send.
        std::thread::sleep(Duration::from_millis(150));

        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender.set_broadcast(true).unwrap();
        let beacon = beacon_payload("PauseResume-Test-Host", 41234, false);

        let dst = SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT));
        let has_test_host = |scanner: &DiscoveryScanner| {
            scanner
                .discovered_hosts()
                .iter()
                .any(|h| h.hostname == "PauseResume-Test-Host")
        };

        // Pause first, then inject beacons (a real beacon repeats every 250 ms).
        // While paused, recv_from is skipped (and any in-flight receive is
        // discarded) so the host must never appear in discovered_hosts().
        scanner.set_paused(true);
        let paused_deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < paused_deadline {
            sender.send_to(beacon.as_bytes(), dst).unwrap();
            assert!(
                !has_test_host(&scanner),
                "host was discovered while the scanner was paused"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // Resume: the scanner drains queued beacons and the host shows up.
        scanner.set_paused(false);
        let resume_deadline = Instant::now() + Duration::from_secs(2);
        let mut discovered = false;
        while Instant::now() < resume_deadline {
            sender.send_to(beacon.as_bytes(), dst).unwrap();
            if has_test_host(&scanner) {
                discovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(discovered, "host was not discovered after the scanner resumed");
    }

    #[test]
    fn test_discovery_scanner_reuse_address() {
        let scanner1 = DiscoveryScanner::start();
        // Spawning a second scanner or socket with reuse_address should not crash or panic
        let raw_sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        );
        if let Ok(sock) = raw_sock {
            let _ = sock.set_reuse_address(true);
            #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
            let _ = sock.set_reuse_port(true);
            let bind_addr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
            let _ = sock.bind(&bind_addr.into());
        }
        drop(scanner1);
    }
}
