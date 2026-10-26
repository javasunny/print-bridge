#![cfg(target_os = "macos")]

use print_bridge::{
    media::{PaperSize, PAPER_SIZES},
    Bridge, SharedPrinter,
};
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
fn bundled_ppd_has_generic_identity_and_outputs_valid_pdf() {
    let ppd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/Print-Bridge-PDF.ppd");
    let content = fs::read_to_string(&ppd).unwrap();
    for identity in [
        "*ModelName: \"Print Bridge PDF\"",
        "*Product: \"(Print Bridge PDF)\"",
        "*ShortNickName: \"Print Bridge PDF\"",
        "*NickName: \"Print Bridge PDF\"",
    ] {
        assert!(content.lines().any(|line| line == identity), "{identity}");
    }
    assert!(content.contains("*DefaultPageSize: A4"));
    assert_eq!(content.matches("*PageSize ").count(), 1);
    assert!(!content.contains("*OpenUI *Duplex"));
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
            duplex: true,
            paper_sizes: PAPER_SIZES.to_vec(),
            default_paper: PaperSize::A4,
            max_copies: 99,
        })
        .unwrap();
    bridge.set_authentication_required(false).unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let uri = format!(
        "ipp://{}/ipp/printers/office",
        bridge.status().address.unwrap()
    );
    let ppd = directory.path().join("Print-Bridge-office.ppd");
    let download = reqwest::blocking::get(format!(
        "http://{}/ipp/printers/office/driver.ppd",
        bridge.status().address.unwrap()
    ))
    .unwrap()
    .error_for_status()
    .unwrap()
    .text()
    .unwrap();
    fs::write(&ppd, download).unwrap();
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
    let mut expected = Vec::new();
    for paper in PAPER_SIZES {
        for (sides, native) in [
            ("one-sided", "None"),
            ("two-sided-long-edge", "DuplexNoTumble"),
            ("two-sided-short-edge", "DuplexTumble"),
        ] {
            expected.push((paper, sides));
            let submitted = Command::new("lp")
                .args([
                    "-d",
                    &queue.0,
                    "-o",
                    &format!("media={}", paper.name()),
                    "-o",
                    &format!("sides={sides}"),
                    "-o",
                    "AP_ColorMatchingMode=AP_ApplicationColorMatching",
                    "-o",
                    "AP_D_InputSlot=",
                    "-o",
                    "ColorModel=Gray",
                    "-o",
                    &format!("Duplex={native}"),
                    "-o",
                    "collate=false",
                    "-o",
                    "pserrorhandler-requested=standard",
                    "-o",
                    "job-priority=50",
                    "-o",
                    "PaperInfoIsSuggested..b.=true",
                    "-o",
                    "PDFIsProtected..b.=false",
                    "-o",
                    "DestinationPrinterID=compatibility-test",
                    "-o",
                    "com.apple.print.PrintSettings.PMCopies..n.=1",
                    "-o",
                    "com.apple.print.JobInfo.PMJobName=private-test-title",
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
                    .filter(|job| job.state == "simulated")
                    .count()
                    == expected.len()
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
                        "Native queue did not reach simulation: {}\nDiagnostics: {:?}",
                        String::from_utf8_lossy(&status.stdout),
                        bridge.connection_events()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    bridge.stop().unwrap();
    let jobs = bridge.list_jobs().unwrap();
    assert_eq!(jobs.len(), expected.len());
    for (job, (paper, sides)) in jobs.iter().rev().zip(expected) {
        assert_eq!(job.device_id, "anonymous");
        assert_eq!(job.state, "simulated");
        assert_eq!(job.paper, paper);
        assert_eq!(job.sides, sides);
    }
    assert!(!serde_json::to_string(&bridge.connection_events())
        .unwrap()
        .contains("private-test-title"));
    assert!(bridge
        .connection_events()
        .iter()
        .any(|event| event.operation == Some(0x0002)
            && event.ipp_status == Some(0)
            && event.client_ip.as_deref() == Some("127.0.0.1")));
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
            duplex: true,
            paper_sizes: PAPER_SIZES.to_vec(),
            default_paper: PaperSize::Letter,
            max_copies: 1,
        })
        .unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let address = bridge.status().address.unwrap();
    let uri = format!("ipp://{address}/ipp/printers/office");
    let probe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("macos/check-ipp-connection.test");
    let output = Command::new("ipptool")
        .args(["-T", "5", "-tv", &uri, probe.to_str().unwrap()])
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
    assert!(bridge
        .connection_events()
        .iter()
        .any(|event| event.operation == Some(0x000b)
            && event.ipp_status == Some(0)
            && event.client_ip.as_deref() == Some("127.0.0.1")));
    assert!(bridge.list_jobs().unwrap().is_empty());
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
    for paper in PAPER_SIZES {
        assert!(
            ppd_text.contains(&format!("*PageSize {}", paper.name())),
            "{ppd_text}"
        );
    }
    assert!(ppd_text.contains("*DefaultPageSize: Letter"), "{ppd_text}");
    assert!(ppd_text.contains("*Duplex DuplexNoTumble"), "{ppd_text}");
    assert!(ppd_text.contains("*Duplex DuplexTumble"), "{ppd_text}");
    let generated_ppd = directory.path().join("generated.ppd");
    fs::write(&generated_ppd, ppd_text).unwrap();
    assert_pdf_output(&generated_ppd);
}

#[test]
fn configured_ppds_pass_cups_validation_and_preserve_pdf_output() {
    let directory = tempfile::tempdir().unwrap();
    for default_paper in PAPER_SIZES {
        for duplex in [false, true] {
            let printer = SharedPrinter {
                id: "office".into(),
                name: "Office".into(),
                queue: "Queue A".into(),
                enabled: true,
                duplex,
                paper_sizes: PAPER_SIZES.to_vec(),
                default_paper,
                max_copies: 3,
            };
            let ppd = directory.path().join("configured.ppd");
            fs::write(&ppd, print_bridge::ppd::generate(&printer)).unwrap();
            assert_pdf_output(&ppd);
        }
    }
}
