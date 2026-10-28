use crate::media::PaperSize;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrinterInfo {
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "DriverName", default)]
    pub driver_name: String,
    #[serde(alias = "PortName", default)]
    pub port_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrintOptions {
    pub copies: u32,
    pub duplex: String,
    #[serde(default)]
    pub paper: PaperSize,
    #[serde(default = "default_color_mode")]
    pub color_mode: String,
}

fn default_color_mode() -> String {
    "auto".into()
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            copies: 1,
            duplex: "one-sided".into(),
            paper: PaperSize::A4,
            color_mode: default_color_mode(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintOutcome {
    Submitted,
    Failed(String),
    Uncertain(String),
}

pub fn build_print_args(
    queue: &str,
    pdf: &Path,
    options: &PrintOptions,
) -> Result<Vec<OsString>, String> {
    if queue.trim().is_empty() || queue.contains('\0') {
        return Err("打印队列名称无效".into());
    }
    if !pdf.is_absolute() {
        return Err("打印文件必须使用服务端生成的绝对路径".into());
    }
    if !(1..=99).contains(&options.copies) {
        return Err("份数必须为 1–99".into());
    }
    let sides = match options.duplex.as_str() {
        "one-sided" => "simplex",
        "two-sided-long-edge" => "duplexlong",
        "two-sided-short-edge" => "duplexshort",
        _ => return Err("不支持的单双面选项".into()),
    };
    let color = match options.color_mode.as_str() {
        "auto" => "",
        "color" => ",color",
        "monochrome" => ",monochrome",
        _ => return Err("不支持的颜色模式".into()),
    };
    Ok(vec![
        "-print-to".into(),
        queue.into(),
        "-print-settings".into(),
        format!(
            "paperkind={},fit,{}x,{sides}{color}",
            options.paper.windows_paper_kind(),
            options.copies
        )
        .into(),
        "-silent".into(),
        pdf.as_os_str().to_owned(),
    ])
}

#[cfg(windows)]
pub fn enumerate_printers() -> Result<Vec<PrinterInfo>, String> {
    use std::mem::size_of;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER};
    use windows_sys::Win32::Graphics::Printing::{
        EnumPrintersW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL,
    };

    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let mut needed = 0u32;
    let mut returned = 0u32;
    let success =
        unsafe { EnumPrintersW(flags, null(), 4, null_mut(), 0, &mut needed, &mut returned) };
    if success != 0 {
        return Ok(Vec::new());
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
        return Err(format!("Windows 打印机枚举失败（错误码 {error}）"));
    }

    loop {
        let requested_bytes =
            usize::try_from(needed).map_err(|_| "Windows 打印机列表过大".to_owned())?;
        let words = requested_bytes
            .checked_add(size_of::<u64>() - 1)
            .ok_or_else(|| "Windows 打印机列表过大".to_owned())?
            / size_of::<u64>();
        let allocated_bytes = words
            .checked_mul(size_of::<u64>())
            .ok_or_else(|| "Windows 打印机列表过大".to_owned())?;
        let buffer_bytes =
            u32::try_from(allocated_bytes).map_err(|_| "Windows 打印机列表过大".to_owned())?;
        let mut buffer = vec![0u64; words];

        let success = unsafe {
            EnumPrintersW(
                flags,
                null(),
                4,
                buffer.as_mut_ptr().cast(),
                buffer_bytes,
                &mut needed,
                &mut returned,
            )
        };
        if success != 0 {
            return parse_printer_info_4(&buffer, allocated_bytes, returned);
        }

        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER || needed <= buffer_bytes {
            return Err(format!("Windows 打印机枚举失败（错误码 {error}）"));
        }
    }
}

#[cfg(not(windows))]
pub fn enumerate_printers() -> Result<Vec<PrinterInfo>, String> {
    Err("真实打印机枚举仅支持 Windows".into())
}

#[cfg(windows)]
fn parse_printer_info_4(
    buffer: &[u64],
    buffer_bytes: usize,
    returned: u32,
) -> Result<Vec<PrinterInfo>, String> {
    use std::mem::{align_of, size_of};
    use windows_sys::Win32::Graphics::Printing::PRINTER_INFO_4W;

    let entry_size = size_of::<PRINTER_INFO_4W>();
    let returned = usize::try_from(returned).map_err(|_| "Windows 打印机列表无效".to_owned())?;
    if buffer_bytes > std::mem::size_of_val(buffer) || returned > buffer_bytes / entry_size {
        return Err("Windows 打印机列表无效".into());
    }

    let base = buffer.as_ptr().cast::<u8>();
    let start = base as usize;
    let end = start
        .checked_add(buffer_bytes)
        .ok_or_else(|| "Windows 打印机列表无效".to_owned())?;
    let entries = base.cast::<PRINTER_INFO_4W>();
    if (entries as usize) % align_of::<PRINTER_INFO_4W>() != 0 {
        return Err("Windows 打印机列表未对齐".into());
    }

    let mut printers = Vec::with_capacity(returned);
    for index in 0..returned {
        let entry = unsafe { &*entries.add(index) };
        let name = read_printer_name(entry.pPrinterName, start, end)?;
        printers.push(PrinterInfo {
            name,
            driver_name: String::new(),
            port_name: String::new(),
        });
    }
    Ok(printers)
}

#[cfg(windows)]
fn read_printer_name(pointer: *const u16, start: usize, end: usize) -> Result<String, String> {
    use std::mem::{align_of, size_of};
    use std::slice;

    let address = pointer as usize;
    if pointer.is_null() || address < start || address >= end || address % align_of::<u16>() != 0 {
        return Err("Windows 打印机名称无效".into());
    }
    let remaining_bytes = end - address;
    let units = remaining_bytes / size_of::<u16>();
    let wide = unsafe { slice::from_raw_parts(pointer, units) };
    let terminator = wide
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| "Windows 打印机名称未终止".to_owned())?;
    String::from_utf16(&wide[..terminator]).map_err(|_| "Windows 打印机名称不是有效 UTF-16".into())
}

#[cfg(windows)]
pub fn print_pdf(
    executable: &Path,
    queue: &str,
    pdf: &Path,
    options: &PrintOptions,
) -> PrintOutcome {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let args = match build_print_args(queue, pdf, options) {
        Ok(args) => args,
        Err(error) => return PrintOutcome::Failed(error),
    };
    if !executable.is_absolute() || !executable.is_file() || !pdf.is_file() {
        return PrintOutcome::Failed("内置打印程序或打印文件不可用".into());
    }
    let mut child = match Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return PrintOutcome::Failed("无法启动内置打印程序".into()),
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return classify_exit(status.code()),
            Ok(None) if started.elapsed() < Duration::from_secs(300) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return PrintOutcome::Uncertain(
                    "打印调用超时；请核查 Windows 队列和出纸情况，禁止自动重试".into(),
                );
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return PrintOutcome::Uncertain(
                    "无法确认打印程序状态；请人工核查，禁止自动重试".into(),
                );
            }
        }
    }
}

#[cfg(not(windows))]
pub fn print_pdf(
    _executable: &Path,
    _queue: &str,
    _pdf: &Path,
    _options: &PrintOptions,
) -> PrintOutcome {
    PrintOutcome::Failed("真实打印仅支持 Windows；当前平台未执行打印".into())
}

#[cfg(any(windows, test))]
fn classify_exit(code: Option<i32>) -> PrintOutcome {
    match code {
        Some(0) => PrintOutcome::Submitted,
        // Until the bundled release is validated, a failure after process launch
        // cannot establish that no pages reached the spooler.
        Some(code) => {
            PrintOutcome::Uncertain(format!("打印程序退出码 {code}；可能已部分提交，请人工核查"))
        }
        None => PrintOutcome::Uncertain("打印程序异常退出，请人工核查".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf() -> std::path::PathBuf {
        std::env::temp_dir().join("中文 invoice 1.pdf")
    }

    #[test]
    fn print_settings_map_all_sides_and_preserve_argument_boundaries() {
        for (paper, paper_kind) in [
            (PaperSize::A3, 8),
            (PaperSize::A4, 9),
            (PaperSize::A5, 11),
            (PaperSize::A6, 70),
            (PaperSize::Letter, 1),
            (PaperSize::Legal, 5),
            (PaperSize::Tabloid, 3),
        ] {
            for (input, expected) in [
                ("one-sided", "simplex"),
                ("two-sided-long-edge", "duplexlong"),
                ("two-sided-short-edge", "duplexshort"),
            ] {
                let args = build_print_args(
                    "Office & Warehouse Printer",
                    &pdf(),
                    &PrintOptions {
                        copies: 3,
                        duplex: input.into(),
                        paper,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(args.len(), 6);
                assert_eq!(args[1], "Office & Warehouse Printer");
                assert_eq!(
                    args[3],
                    OsString::from(format!("paperkind={paper_kind},fit,3x,{expected}"))
                );
                assert_eq!(args[5], pdf().as_os_str());
                assert!(!args.iter().any(|arg| arg == "-print-to-default"));
            }
        }
    }

    #[test]
    fn rejects_unsupported_options_before_starting_a_process() {
        for copies in [0, 100, u32::MAX] {
            assert!(build_print_args(
                "printer",
                &pdf(),
                &PrintOptions {
                    copies,
                    ..Default::default()
                }
            )
            .is_err());
        }
        assert!(build_print_args(
            "printer",
            &pdf(),
            &PrintOptions {
                copies: 1,
                duplex: "simplex,2x".into(),
                paper: PaperSize::A4,
                ..Default::default()
            }
        )
        .is_err());
        assert!(build_print_args("", &pdf(), &PrintOptions::default()).is_err());
        assert!(build_print_args(
            "printer",
            Path::new("-untrusted.pdf"),
            &PrintOptions::default()
        )
        .is_err());
    }

    #[test]
    fn explicit_color_modes_reach_the_driver_and_cannot_inject_print_settings() {
        for mode in ["color", "monochrome"] {
            let options = PrintOptions {
                color_mode: mode.into(),
                ..Default::default()
            };
            let args = build_print_args("Exact Queue", &pdf(), &options).unwrap();
            assert_eq!(
                args[3],
                OsString::from(format!("paperkind=9,fit,1x,simplex,{mode}"))
            );
            assert_eq!(args[1], "Exact Queue");
        }
        let invalid = PrintOptions {
            color_mode: "color,100x".into(),
            ..Default::default()
        };
        assert!(build_print_args("Exact Queue", &pdf(), &invalid).is_err());
        let legacy: PrintOptions =
            serde_json::from_str(r#"{"copies":1,"duplex":"one-sided","paper":"A4"}"#).unwrap();
        assert_eq!(legacy.color_mode, "auto");
    }

    #[cfg(windows)]
    #[test]
    fn parses_level_four_printer_names_from_an_aligned_utf16_buffer() {
        use std::mem::{size_of, size_of_val};
        use std::ptr::{copy_nonoverlapping, null_mut, write};
        use windows_sys::Win32::Graphics::Printing::PRINTER_INFO_4W;

        let mut name: Vec<u16> = "办公室 🖨".encode_utf16().collect();
        name.push(0);
        let data_bytes = size_of::<PRINTER_INFO_4W>() + name.len() * size_of::<u16>();
        let mut buffer = vec![0u64; data_bytes.div_ceil(size_of::<u64>())];
        let base = buffer.as_mut_ptr().cast::<u8>();
        let name_pointer = unsafe { base.add(size_of::<PRINTER_INFO_4W>()).cast::<u16>() };
        unsafe {
            copy_nonoverlapping(name.as_ptr(), name_pointer, name.len());
            write(
                base.cast::<PRINTER_INFO_4W>(),
                PRINTER_INFO_4W {
                    pPrinterName: name_pointer,
                    pServerName: null_mut(),
                    Attributes: 0,
                },
            );
        }

        let printers = parse_printer_info_4(&buffer, size_of_val(buffer.as_slice()), 1).unwrap();
        assert_eq!(printers.len(), 1);
        assert_eq!(printers[0].name, "办公室 🖨");
        assert!(printers[0].driver_name.is_empty());
        assert!(printers[0].port_name.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unterminated_level_four_printer_name() {
        use std::mem::size_of;
        use std::ptr::{null_mut, write};
        use windows_sys::Win32::Graphics::Printing::PRINTER_INFO_4W;

        let data_bytes = size_of::<PRINTER_INFO_4W>() + size_of::<u16>();
        let mut buffer = vec![0u64; data_bytes.div_ceil(size_of::<u64>())];
        let base = buffer.as_mut_ptr().cast::<u8>();
        let name_pointer = unsafe { base.add(size_of::<PRINTER_INFO_4W>()).cast::<u16>() };
        unsafe {
            name_pointer.write(b'A' as u16);
            write(
                base.cast::<PRINTER_INFO_4W>(),
                PRINTER_INFO_4W {
                    pPrinterName: name_pointer,
                    pServerName: null_mut(),
                    Attributes: 0,
                },
            );
        }

        assert!(parse_printer_info_4(&buffer, data_bytes, 1).is_err());
    }

    #[test]
    fn a_failed_process_is_never_assumed_safe_to_retry() {
        assert_eq!(classify_exit(Some(0)), PrintOutcome::Submitted);
        for code in [Some(1), Some(2), Some(5), None] {
            assert!(matches!(classify_exit(code), PrintOutcome::Uncertain(_)));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_never_claims_real_print_success() {
        assert!(enumerate_printers().is_err());
        assert!(matches!(
            print_pdf(
                Path::new("SumatraPDF.exe"),
                "Office",
                &pdf(),
                &PrintOptions::default()
            ),
            PrintOutcome::Failed(_)
        ));
    }
}
