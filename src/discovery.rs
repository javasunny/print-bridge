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
    time::Duration,
};

const SERVICE_TYPE: &str = "_universal._sub._ipp._tcp.local.";

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
}

impl Discovery {
    pub(crate) fn start(
        printers: &[SharedPrinter],
        address: SocketAddr,
        auth: bool,
    ) -> Result<Self, String> {
        let mut discovery = Self {
            daemon: None,
            names: Vec::new(),
            error: Arc::new(Mutex::new(None)),
            announced: Arc::new(AtomicBool::new(false)),
        };
        if address.ip().is_loopback() || !printers.iter().any(|p| p.enabled) {
            return Ok(discovery);
        }
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        let events = daemon.monitor().map_err(|e| e.to_string())?;
        discovery.daemon = Some(daemon.clone());
        for printer in printers.iter().filter(|p| p.enabled) {
            let info = service_info(printer, address, auth)?;
            let name = info.get_fullname().to_owned();
            daemon.register(info).map_err(|e| e.to_string())?;
            discovery.names.push(name);
        }
        let error = Arc::clone(&discovery.error);
        let announced = Arc::clone(&discovery.announced);
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    DaemonEvent::Error(message) => {
                        *error.lock().unwrap() = Some(message.to_string())
                    }
                    DaemonEvent::Announce(..) => announced.store(true, Ordering::SeqCst),
                    _ => (),
                }
            }
        });
        Ok(discovery)
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
