#![cfg(target_os = "macos")]

use print_bridge::{Bridge, SharedPrinter};
use std::{fs, path::PathBuf, process::Command};

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
    let filtered = Command::new("cupsfilter")
        .args([
            "-e",
            "-p",
            generated_ppd.to_str().unwrap(),
            "-m",
            "printer/office",
        ])
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
        "generated PPD emitted a non-PDF document: {}",
        String::from_utf8_lossy(&filtered.stderr)
    );
}
