#![cfg(target_os = "macos")]

use print_bridge::{
    media::{PaperSize, PAPER_SIZES},
    Bridge, SharedPrinter,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
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
        let _ = Command::new("cancel").args(["-a", &self.0]).output();
        let _ = Command::new("lpadmin").args(["-x", &self.0]).output();
    }
}

struct BridgeShutdown(Arc<Bridge>);
impl Drop for BridgeShutdown {
    fn drop(&mut self) {
        let _ = self.0.stop();
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
fn native_cupsfilter_urf_and_pwg_raster_normalize_to_single_page_pdf() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
    bridge
        .save_printer(SharedPrinter {
            id: "office".into(),
            name: "Office".into(),
            queue: "Raster Fixture Queue".into(),
            enabled: true,
            duplex: true,
            paper_sizes: PAPER_SIZES.to_vec(),
            default_paper: PaperSize::Letter,
            max_copies: 9,
        })
        .unwrap();
    bridge.set_authentication_required(false).unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let _shutdown = BridgeShutdown(Arc::clone(&bridge));
    let address = bridge.status().address.unwrap();
    let printer_uri = format!("ipp://{address}/ipp/printers/office");
    let endpoint = format!("http://{address}/ipp/printers/office");
    let client = reqwest::blocking::Client::new();
    let compressed_pdf = Path::new("/usr/share/cups/ipptool/document-a4.pdf");
    let decompressed = Command::new("gzip")
        .args(["-dc"])
        .arg(compressed_pdf)
        .output()
        .unwrap();
    assert!(
        decompressed.status.success(),
        "gzip failed: {}",
        String::from_utf8_lossy(&decompressed.stderr)
    );
    assert!(decompressed.stdout.starts_with(b"%PDF-"));
    let pdf_path = directory.path().join("document.pdf");
    fs::write(&pdf_path, decompressed.stdout).unwrap();

    let airprint_ppd = Path::new(
        "/System/Library/Frameworks/ApplicationServices.framework/Versions/A/Frameworks/PrintCore.framework/Versions/A/Resources/AirPrint.ppd",
    );
    for (index, mime) in ["image/urf", "image/pwg-raster"].into_iter().enumerate() {
        let raster = Command::new("cupsfilter")
            .args([
                "-P",
                airprint_ppd.to_str().unwrap(),
                "-e",
                "-i",
                "application/pdf",
                "-m",
                mime,
            ])
            .args([
                "-o",
                "PageSize=Letter",
                "-o",
                "Resolution=300dpi",
                "-o",
                "page-ranges=1",
            ])
            .arg(&pdf_path)
            .output()
            .unwrap();
        assert!(
            raster.status.success(),
            "cupsfilter failed for {mime}: {}",
            String::from_utf8_lossy(&raster.stderr)
        );
        assert!(
            !raster.stdout.is_empty(),
            "cupsfilter emitted no {mime} data"
        );

        let mut operation = print_bridge::ipp::operation_attributes();
        operation.attributes.extend([
            print_bridge::ipp::Attribute::text(print_bridge::ipp::URI, "printer-uri", &printer_uri),
            print_bridge::ipp::Attribute::text(
                print_bridge::ipp::MIME_MEDIA_TYPE,
                "document-format",
                mime,
            ),
        ]);
        let job = print_bridge::ipp::Group::new(
            print_bridge::ipp::JOB_ATTRIBUTES,
            vec![
                print_bridge::ipp::Attribute::integer(print_bridge::ipp::INTEGER, "copies", 1),
                print_bridge::ipp::Attribute::text(
                    print_bridge::ipp::KEYWORD,
                    "sides",
                    "one-sided",
                ),
                print_bridge::ipp::Attribute::text(
                    print_bridge::ipp::KEYWORD,
                    "media",
                    PaperSize::Letter.keyword(),
                ),
            ],
        );
        let request = print_bridge::ipp::encode(
            (2, 0),
            print_bridge::ipp::PRINT_JOB,
            index as i32 + 1,
            &[operation, job],
            &raster.stdout,
        )
        .unwrap();
        let response = client
            .post(&endpoint)
            .header("Content-Type", "application/ipp")
            .body(request)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        let response_body = response.bytes().unwrap();
        let response = print_bridge::ipp::parse(&response_body).unwrap();
        assert_eq!(response.code, print_bridge::ipp::SUCCESSFUL_OK, "{mime}");

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let jobs = bridge.list_jobs().unwrap();
            if jobs.len() == index + 1 && jobs.iter().all(|job| job.state == "simulated") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{mime} did not reach simulation: jobs={jobs:?}, events={:?}",
                bridge.connection_events()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    bridge.stop().unwrap();
    let jobs = bridge.list_jobs().unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|job| {
        job.state == "simulated"
            && job.copies == 1
            && job.paper == PaperSize::Letter
            && job.sides == "one-sided"
    }));
    let documents = directory.path().join("simulation/documents");
    let files: Vec<_> = fs::read_dir(documents)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    for path in files {
        let pdf = lopdf::Document::load(&path).unwrap();
        assert_eq!(pdf.get_pages().len(), 1, "{}", path.display());
    }
    let print_events: Vec<_> = bridge
        .connection_events()
        .into_iter()
        .filter(|event| event.operation == Some(print_bridge::ipp::PRINT_JOB))
        .collect();
    assert_eq!(print_events.len(), 2);
    assert!(print_events.iter().all(|event| {
        event.http_status == 200
            && event.ipp_status == Some(print_bridge::ipp::SUCCESSFUL_OK)
            && event.client_ip.as_deref() == Some("127.0.0.1")
    }));
}

#[test]
#[ignore = "Creates and removes a temporary IPP Everywhere CUPS queue; requires printer administration access"]
fn native_driverless_queue_submits_requested_options_to_simulation() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
    bridge
        .save_printer(SharedPrinter {
            id: "office".into(),
            name: "Office".into(),
            queue: "Driverless Queue".into(),
            enabled: true,
            duplex: true,
            paper_sizes: PAPER_SIZES.to_vec(),
            default_paper: PaperSize::A4,
            max_copies: 9,
        })
        .unwrap();
    bridge.set_authentication_required(false).unwrap();
    bridge.start("127.0.0.1:0").unwrap();
    let uri = format!(
        "ipp://{}/ipp/printers/office",
        bridge.status().address.unwrap()
    );
    let queue = TemporaryQueue(format!(
        "PrintBridge_Driverless_Test_{}",
        std::process::id()
    ));
    // macOS lpadmin's supported driverless model is `everywhere`; this makes
    // CUPS query the IPP printer and build the queue without a supplied PPD.
    let configured = Command::new("lpadmin")
        .args([
            "-p",
            &queue.0,
            "-E",
            "-v",
            &uri,
            "-m",
            "everywhere",
            "-o",
            "printer-is-shared=false",
        ])
        .output()
        .unwrap();
    assert!(
        configured.status.success(),
        "lpadmin -m everywhere failed: {}\n{}\nBridge IPP events: {:?}",
        String::from_utf8_lossy(&configured.stdout),
        String::from_utf8_lossy(&configured.stderr),
        bridge.connection_events()
    );

    let submitted = Command::new("lp")
        .args([
            "-d",
            &queue.0,
            "-n",
            "3",
            "-o",
            "media=na_letter_8.5x11in",
            "-o",
            "sides=two-sided-short-edge",
            "/usr/share/cups/ipptool/document-a4.pdf",
        ])
        .output()
        .unwrap();
    assert!(
        submitted.status.success(),
        "lp submission failed: {}\n{}",
        String::from_utf8_lossy(&submitted.stdout),
        String::from_utf8_lossy(&submitted.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let jobs = loop {
        let jobs = bridge.list_jobs().unwrap();
        if jobs.len() == 1 && jobs[0].state == "simulated" {
            break jobs;
        }
        if Instant::now() >= deadline {
            let status = Command::new("lpstat")
                .args(["-p", &queue.0, "-l"])
                .output()
                .unwrap();
            bridge.stop().unwrap();
            panic!(
                "Driverless job did not reach simulation: {}\nIPP events: {:?}",
                String::from_utf8_lossy(&status.stdout),
                bridge.connection_events()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    bridge.stop().unwrap();

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].device_id, "anonymous");
    assert_eq!(jobs[0].state, "simulated");
    assert_eq!(jobs[0].copies, 3);
    assert_eq!(jobs[0].sides, "two-sided-short-edge");
    assert_eq!(jobs[0].paper, PaperSize::Letter);

    let events = bridge.connection_events();
    let job_operations: Vec<_> = events
        .iter()
        .filter_map(|event| event.operation)
        .filter(|operation| matches!(operation, 0x0002 | 0x0005 | 0x0006))
        .collect();
    let print_job = job_operations.contains(&0x0002);
    let create_and_send = job_operations.contains(&0x0005) && job_operations.contains(&0x0006);
    assert!(
        print_job || create_and_send,
        "CUPS did not submit Print-Job or Create-Job/Send-Document: {events:?}"
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(event.operation, Some(0x0002 | 0x0005 | 0x0006)))
            .all(|event| event.ipp_status == Some(0)
                && event.http_status == 200
                && event.client_ip.as_deref() == Some("127.0.0.1")),
        "Native job operation failed: {events:?}"
    );
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
        "{}\n{}\nipp2ppd status: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        generated.status,
        String::from_utf8_lossy(&generated.stderr)
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
