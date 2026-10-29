use crate::SharedPrinter;
use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const SERVICE_TYPE: &str = "_universal._sub._ipp._tcp.local.";
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn machine_identity() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .or_else(|_| crate::network::default_listen_address())
            .unwrap_or_else(|_| "print-bridge".into())
    })
}

pub(crate) fn printer_uuid(id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(machine_identity());
    digest.update([0]);
    digest.update(id);
    let mut bytes: [u8; 16] = digest.finalize()[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn service_info(p: &SharedPrinter, address: SocketAddr, auth: bool) -> Result<ServiceInfo, String> {
    let uuid = printer_uuid(&p.id);
    let suffix = &uuid[..8];
    let mut name: String = p.name.chars().take_while(|c| !c.is_control()).collect();
    while name.len() > 48 {
        name.pop();
    }
    let name = format!("{name} ({suffix})");
    let host = format!(
        "print-bridge-{}.local.",
        &format!("{:x}", Sha256::digest(machine_identity()))[..12]
    );
    let properties: HashMap<String, String> = [
        ("txtvers", "1".into()),
        ("qtotal", "1".into()),
        ("rp", format!("ipp/printers/{}", p.id)),
        ("ty", "Print Bridge".into()),
        ("product", "(Print Bridge)".into()),
        (
            "pdl",
            "application/pdf,image/urf,image/pwg-raster,image/jpeg".into(),
        ),
        ("URF", crate::ipp::urf_supported(p.duplex)),
        ("UUID", uuid),
        ("Color", "T".into()),
        ("Duplex", if p.duplex { "T" } else { "F" }.into()),
        ("Copies", "T".into()),
        ("Collate", "F".into()),
        ("kind", "document,photo".into()),
        ("priority", "0".into()),
        (
            "air",
            if auth { "username,password" } else { "none" }.into(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &name,
        &host,
        if address.ip().is_unspecified() {
            String::new()
        } else {
            address.ip().to_string()
        },
        address.port(),
        properties,
    )
    .map_err(|e| e.to_string())?;
    if address.is_ipv4() {
        info.set_interfaces(vec![IfKind::IPv4]);
    }
    Ok(if address.ip().is_unspecified() {
        info.enable_addr_auto()
    } else {
        info
    })
}

pub(crate) struct Discovery {
    daemon: Option<ServiceDaemon>,
    names: Vec<String>,
    error: Arc<Mutex<Option<String>>>,
    announced: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    monitor: Option<std::thread::JoinHandle<()>>,
}

impl Discovery {
    pub(crate) fn start(
        printers: &[SharedPrinter],
        address: SocketAddr,
        auth: bool,
    ) -> Result<Self, String> {
        let discovery = Self {
            daemon: None,
            names: Vec::new(),
            error: Arc::new(Mutex::new(None)),
            announced: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            monitor: None,
        };
        if address.ip().is_loopback() || !printers.iter().any(|p| p.enabled) {
            return Ok(discovery);
        }
        let services = printers
            .iter()
            .filter(|p| p.enabled)
            .map(|printer| service_info(printer, address, auth))
            .collect::<Result<Vec<_>, _>>()?;
        discovery.register_services(
            ServiceDaemon::new().map_err(|e| e.to_string())?,
            services,
            ANNOUNCE_INTERVAL,
        )
    }

    fn register_services(
        mut self,
        daemon: ServiceDaemon,
        services: Vec<ServiceInfo>,
        interval: Duration,
    ) -> Result<Self, String> {
        self.daemon = Some(daemon.clone());
        let events = daemon.monitor().map_err(|e| e.to_string())?;
        for info in &services {
            daemon.register(info.clone()).map_err(|e| e.to_string())?;
            self.names.push(info.get_fullname().to_owned());
        }
        let error = Arc::clone(&self.error);
        let announced = Arc::clone(&self.announced);
        let stopped = Arc::clone(&self.stopped);
        self.monitor = Some(std::thread::spawn(move || {
            let mut next_announce = Instant::now() + interval;
            while !stopped.load(Ordering::SeqCst) {
                // Use a deadline so query traffic cannot postpone the refresh.
                let now = Instant::now();
                if now >= next_announce {
                    for info in &services {
                        // Re-register in place: no goodbye or gap in discovery.
                        // Keep the original auto-address template to avoid stale IPs.
                        if let Err(message) = daemon.register(info.clone()) {
                            *error.lock().unwrap() = Some(message.to_string());
                            announced.store(false, Ordering::SeqCst);
                        }
                    }
                    next_announce = now + interval;
                }
                let wait = next_announce
                    .saturating_duration_since(Instant::now())
                    .min(MONITOR_POLL_INTERVAL);
                match events.recv_timeout(wait) {
                    Ok(DaemonEvent::Error(message)) => {
                        *error.lock().unwrap() = Some(message.to_string());
                        announced.store(false, Ordering::SeqCst);
                    }
                    Ok(DaemonEvent::Announce(..)) => {
                        *error.lock().unwrap() = None;
                        announced.store(true, Ordering::SeqCst);
                    }
                    Ok(_) => (),
                    Err(_) if events.is_disconnected() => {
                        *error.lock().unwrap() = Some("自动发现广播线程已停止".into());
                        announced.store(false, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => (),
                }
            }
        }));
        Ok(self)
    }

    pub(crate) fn active(&self) -> bool {
        self.announced.load(Ordering::SeqCst) && self.error().is_none()
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
        if let Some(daemon) = self.daemon.take() {
            for name in &self.names {
                if let Ok(done) = daemon.unregister(name) {
                    let _ = done.recv_timeout(Duration::from_millis(500));
                }
            }
            if let Ok(done) = daemon.shutdown() {
                let _ = done.recv_timeout(Duration::from_millis(500));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::PaperSize;

    fn printer() -> SharedPrinter {
        SharedPrinter {
            id: "office".into(),
            name: "办公打印机".repeat(30),
            queue: "Queue".into(),
            enabled: true,
            duplex: true,
            max_copies: 3,
            paper_sizes: vec![PaperSize::A4],
            default_paper: PaperSize::A4,
        }
    }

    #[test]
    fn advertises_airprint_subtype_real_queue_and_supported_formats() {
        let p = printer();
        let info = service_info(&p, "192.168.1.8:8631".parse().unwrap(), true).unwrap();
        assert_eq!(info.get_type(), "_ipp._tcp.local.");
        assert_eq!(info.get_subtype().as_deref(), Some(SERVICE_TYPE));
        assert_eq!(info.get_property_val_str("rp"), Some("ipp/printers/office"));
        assert_eq!(info.get_property_val_str("air"), Some("username,password"));
        assert_eq!(
            info.get_property_val_str("URF"),
            Some(crate::ipp::urf_supported(true).as_str())
        );
        assert_eq!(
            info.get_property_val_str("UUID"),
            Some(printer_uuid(&p.id).as_str())
        );
        assert!(info
            .get_property_val_str("pdl")
            .unwrap()
            .contains("image/urf"));
        assert_eq!(info.get_addresses().len(), 1);
        assert!(!info.is_addr_auto());
        assert_eq!(info.get_port(), 8631);
        assert!(ANNOUNCE_INTERVAL.as_secs() < u64::from(info.get_host_ttl()));
        assert!(ANNOUNCE_INTERVAL.as_secs() < u64::from(info.get_other_ttl()));
        assert!(info.get_fullname().len() <= 80);
    }

    #[test]
    fn wildcard_listener_tracks_interfaces_and_anonymous_auth_is_honest() {
        let info = service_info(&printer(), "0.0.0.0:12345".parse().unwrap(), false).unwrap();
        assert!(info.is_addr_auto());
        assert_eq!(info.get_property_val_str("air"), Some("none"));
        assert!(info.get_addresses().is_empty());
        assert_ne!(printer_uuid("office"), printer_uuid("warehouse"));
        assert_eq!(printer_uuid("office"), printer_uuid("office"));
    }

    #[test]
    fn loopback_simulations_do_not_publish_network_printers() {
        let discovery =
            Discovery::start(&[printer()], "127.0.0.1:12345".parse().unwrap(), false).unwrap();
        assert!(!discovery.active());
        assert_eq!(discovery.error(), None);
    }

    #[test]
    fn late_queries_are_answered_after_startup_announcements_without_reregistering() {
        use std::net::{Ipv4Addr, UdpSocket};
        use std::time::Instant;

        let port = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let daemon = ServiceDaemon::new_with_port(port).unwrap();
        let p = printer();
        let mut info = service_info(&p, "127.0.0.1:8631".parse().unwrap(), false).unwrap();
        info.set_interfaces(vec![IfKind::LoopbackV4]);
        let _cleanup = Discovery {
            names: vec![info.get_fullname().to_owned()],
            daemon: Some(daemon.clone()),
            error: Arc::new(Mutex::new(None)),
            announced: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            monitor: None,
        };
        daemon.register(info).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let metrics = daemon
                .get_metrics()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            if metrics.get("register-resend").copied().unwrap_or(0) >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "startup announcements did not finish"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        for name in ["_ipp._tcp.local.", SERVICE_TYPE] {
            // Create the client only after both startup announcements: it cannot
            // discover this queue from cached or unsolicited startup packets.
            let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut query = vec![0x43, 0x50, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
            for label in name.trim_end_matches('.').split('.') {
                query.push(label.len() as u8);
                query.extend_from_slice(label.as_bytes());
            }
            query.extend_from_slice(&[0, 0, 12, 0, 1]);
            client.send_to(&query, (Ipv4Addr::LOCALHOST, port)).unwrap();
            let mut buffer = [0; 9000];
            let (size, source) = client.recv_from(&mut buffer).unwrap();
            assert_eq!(source.port(), port);
            assert_eq!(&buffer[..2], &[0x43, 0x50]);
            assert_ne!(buffer[2] & 0x80, 0);
            assert!(buffer[..size]
                .windows(b"rp=ipp/printers/office".len())
                .any(|value| value == b"rp=ipp/printers/office"));
        }
        let metrics = daemon
            .get_metrics()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(metrics.get("register"), Some(&1));
        assert!(metrics.get("respond").copied().unwrap_or(0) >= 2);
    }

    #[test]
    fn refreshes_all_queues_without_queries_and_stops_before_withdrawing() {
        use std::collections::HashSet;
        use std::net::UdpSocket;

        let port = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let daemon = ServiceDaemon::new_with_port(port).unwrap();
        let events = daemon.monitor().unwrap();
        let mut second = printer();
        second.id = "warehouse".into();
        let services: Vec<_> = [printer(), second]
            .iter()
            .map(|p| {
                let mut info = service_info(p, "127.0.0.1:8631".parse().unwrap(), false).unwrap();
                info.set_interfaces(vec![IfKind::LoopbackV4]);
                info
            })
            .collect();
        let names: HashSet<_> = services
            .iter()
            .map(|info| info.get_fullname().to_owned())
            .collect();
        let discovery = Discovery::start(&[], "127.0.0.1:8631".parse().unwrap(), false)
            .unwrap()
            .register_services(daemon.clone(), services, Duration::from_secs(2))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut announcements = HashMap::new();
        let mut refreshed = HashSet::new();
        while refreshed != names {
            assert!(Instant::now() < deadline, "missing periodic announcements");
            if let Ok(DaemonEvent::Announce(name, _)) =
                events.recv_timeout(Duration::from_millis(100))
            {
                let count = announcements.entry(name.clone()).or_insert(0);
                *count += 1;
                // Initial registration sends two announcements. A third must
                // come from the refresh worker, with no client queries.
                if *count >= 3 {
                    refreshed.insert(name);
                }
            }
        }
        assert!(discovery.active());
        let metrics = daemon
            .get_metrics()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(metrics.get("register").copied().unwrap_or(0) >= 4);
        assert_eq!(metrics.get("unregister").copied().unwrap_or(0), 0);
        assert_eq!(metrics.get("respond").copied().unwrap_or(0), 0);
        let worker = Arc::downgrade(&discovery.stopped);
        drop(discovery);
        assert!(
            worker.upgrade().is_none(),
            "refresh worker outlived discovery"
        );
        assert!(daemon.get_metrics().is_err(), "daemon was not shut down");
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "Publishes a temporary Bonjour service on the LAN and uses the native macOS browser"]
    fn native_bonjour_finds_and_withdraws_airprint_service() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut p = printer();
        p.id = format!("discovery-test-{}", std::process::id());
        p.name = "Print Bridge Discovery Test".into();
        let discovery = Discovery::start(&[p.clone()], address, false).unwrap();
        let browse = || {
            std::process::Command::new("ippfind")
                .args([
                    "-T",
                    "5",
                    "_ipp._tcp,_universal",
                    "--port",
                    &address.port().to_string(),
                    "--txt-rp",
                    &format!("^ipp/printers/{}$", p.id),
                    "--print",
                ])
                .output()
                .unwrap()
        };
        let found = browse();
        assert!(
            found.status.success(),
            "{}; {:?}",
            String::from_utf8_lossy(&found.stderr),
            discovery.error()
        );
        assert!(String::from_utf8_lossy(&found.stdout).contains(&format!("/ipp/printers/{}", p.id)));
        drop(discovery);
        let removed = browse();
        assert!(
            removed.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&removed.stdout)
        );
    }
}
