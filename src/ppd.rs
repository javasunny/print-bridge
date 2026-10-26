use crate::SharedPrinter;
use std::fmt::Write;

pub fn generate(printer: &SharedPrinter) -> String {
    let template = include_str!("../macos/Print-Bridge-PDF.ppd");
    let mut ppd = template.split("*OpenUI *PageSize").next().unwrap().replace(
        "*cupsMaxCopies: 99",
        &format!("*cupsMaxCopies: {}", printer.max_copies),
    );
    let default = printer.default_paper.name();
    for option in ["PageSize", "PageRegion"] {
        writeln!(ppd, "*OpenUI *{option}/Media Size: PickOne\n*OrderDependency: 10 AnySetup *{option}\n*Default{option}: {default}").unwrap();
        for paper in &printer.paper_sizes {
            let (width, height) = paper.dimensions();
            let (width, height) = (
                f64::from(width) * 72.0 / 2540.0,
                f64::from(height) * 72.0 / 2540.0,
            );
            writeln!(ppd, "*{option} {}: \"<</PageSize[{width:.4} {height:.4}]/ImagingBBox null/Orientation 0>>setpagedevice\"", paper.name()).unwrap();
        }
        writeln!(ppd, "*CloseUI: *{option}").unwrap();
    }
    writeln!(
        ppd,
        "*DefaultImageableArea: {default}\n*DefaultPaperDimension: {default}"
    )
    .unwrap();
    for paper in &printer.paper_sizes {
        let (width, height) = paper.dimensions();
        let (width, height) = (
            f64::from(width) * 72.0 / 2540.0,
            f64::from(height) * 72.0 / 2540.0,
        );
        writeln!(ppd, "*PaperDimension {}: \"{width:.4} {height:.4}\"\n*ImageableArea {}: \"0.0000 0.0000 {width:.4} {height:.4}\"", paper.name(), paper.name()).unwrap();
    }
    if printer.duplex {
        ppd.push_str("*OpenUI *Duplex/Two-Sided Printing: PickOne\n*OrderDependency: 10 AnySetup *Duplex\n*DefaultDuplex: None\n*Duplex None/Off: \"<</Duplex false>>setpagedevice\"\n*Duplex DuplexNoTumble/Long-Edge: \"<</Duplex true/Tumble false>>setpagedevice\"\n*Duplex DuplexTumble/Short-Edge: \"<</Duplex true/Tumble true>>setpagedevice\"\n*CloseUI: *Duplex\n");
    }
    ppd
}
