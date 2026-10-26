#![cfg(target_os = "macos")]

use print_bridge::{Bridge, SharedPrinter};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn assert_pdf_output(ppd: &Path) {
    let checked = Command::new("cupstestppd")
        .arg("-q")
        .arg(ppd)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    let filtered = Command::new("cupsfilter")
        .args(["-e", "-p", ppd.to_str().unwrap(), "-m", "printer/office"])
        .arg("/usr/share/cups/ipptool/document-a4.pdf")
        .output()
        .unwrap();
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    assert!(
        filtered.stdout.starts_with(b"%PDF-"),
        "PPD emitted a non-PDF document: {}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let pdf = lopdf::Document::load_mem(&filtered.stdout).unwrap();
    assert_eq!(pdf.get_pages().len(), 4);
}

#[test]
fn bundled_single_sided_ppd_outputs_valid_pdf() {
    let ppd =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/Print-Bridge-PDF-A4-Simplex.ppd");
    assert_pdf_output(&ppd);
}

struct TemporaryQueue(String);
impl Drop for TemporaryQueue {
    fn drop(&mut self) {
        let _ = Command::new("lpadmin").args(["-x", &self.0]).output();
    }
}

#[test]
#[ignore = "Creates and removes a temporary local CUPS queue; requires printer administration access"]
fn native_macos_queue_prints_pdf_without_authentication_in_simulation() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
    bridge
        .save_printer(SharedPrinter {
            id: "office".into(),
            name: "Office".into(),
            queue: "Simulated Queue".into(),
            enabled: true,
            duplex: false,
            max_copies: 99,
        })
        .unwrap();
    bridge.set_authentication_required(false).unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let uri = format!(
        "ipp://{}/ipp/printers/office",
        bridge.status().address.unwrap()
    );
    let ppd =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/Print-Bridge-PDF-A4-Simplex.ppd");
    let queue = TemporaryQueue(format!(
        "PrintBridge_Compatibility_Test_{}",
        std::process::id()
    ));
    let configured = Command::new("lpadmin")
        .args([
            "-p",
            &queue.0,
            "-E",
            "-v",
            &uri,
            "-P",
            ppd.to_str().unwrap(),
            "-o",
            "printer-is-shared=false",
            "-o",
            "auth-info-required=none",
        ])
        .output()
        .unwrap();
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let submitted = Command::new("lp")
        .args([
            "-d",
            &queue.0,
            "-o",
            "media=A4",
            "-o",
            "sides=one-sided",
            "/usr/share/cups/ipptool/document-a4.pdf",
        ])
        .output()
        .unwrap();
    assert!(
        submitted.status.success(),
        "{}",
        String::from_utf8_lossy(&submitted.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if bridge
            .list_jobs()
            .unwrap()
            .iter()
            .any(|job| job.state == "simulated")
        {
            break;
        }
        if Instant::now() >= deadline {
            let status = Command::new("lpstat")
                .args(["-p", &queue.0, "-l"])
                .output()
                .unwrap();
            bridge.stop().unwrap();
            panic!(
                "Native queue did not reach simulation: {}",
                String::from_utf8_lossy(&status.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bridge.stop().unwrap();
    let jobs = bridge.list_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].device_id, "anonymous");
    assert_eq!(jobs[0].state, "simulated");
}

#[test]
fn macos_can_probe_pdf_capabilities_without_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
    bridge
        .save_printer(SharedPrinter {
            id: "office".into(),
            name: "Office".into(),
            queue: "Queue A".into(),
            enabled: true,
            duplex: false,
            max_copies: 1,
        })
        .unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let address = bridge.status().address.unwrap();
    let uri = format!("ipp://{address}/ipp/printers/office");
    let probe = directory.path().join("probe.test");
    fs::write(
        &probe,
        "{ OPERATION Get-Printer-Attributes GROUP operation ATTR charset attributes-charset utf-8 ATTR language attributes-natural-language en ATTR uri printer-uri $uri STATUS successful-ok EXPECT document-format-supported }",
    )
    .unwrap();
    let output = Command::new("ipptool")
        .args(["-T", "5", "-V", "1.1", "-tv", &uri, probe.to_str().unwrap()])
        .output()
        .unwrap();
    let ppd = directory.path().join("AirPrint.ppd");
    fs::copy("/System/Library/Frameworks/ApplicationServices.framework/Versions/A/Frameworks/PrintCore.framework/Versions/A/Resources/AirPrint.ppd", &ppd).unwrap();
    let generated = Command::new("/System/Library/Printers/Libraries/ipp2ppd")
        .arg(&uri)
        .arg(&ppd)
        .output()
        .unwrap();
    bridge.stop().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("application/pdf"));
    assert!(
        generated.status.success(),
        "ipp2ppd: {}\n{}",
        String::from_utf8_lossy(&generated.stdout),
        String::from_utf8_lossy(&generated.stderr)
    );
    let ppd_text = String::from_utf8(generated.stdout).unwrap();
    let filters: Vec<_> = ppd_text
        .lines()
        .filter(|line| line.contains("cupsFilter"))
        .collect();
    assert!(
        filters.iter().any(|line| line.contains("application/pdf")),
        "generated filters: {filters:?}; ppd: {}; stderr: {}",
        ppd_text,
        String::from_utf8_lossy(&generated.stderr)
    );
    assert!(ppd_text.contains("*PageSize A4"), "{ppd_text}");
    let generated_ppd = directory.path().join("generated.ppd");
    fs::write(&generated_ppd, ppd_text).unwrap();
    assert_pdf_output(&generated_ppd);
}
