use print_bridge::{
    download::{DownloadStatus, SumatraManager},
    printing::{self, PrinterInfo},
    Bridge, CreatedDevice, Device, Job, ServiceStatus, SharedPrinter,
};
use std::sync::Arc;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};

type Backend<'a> = State<'a, Arc<Bridge>>;

#[tauri::command]
fn service_status(bridge: Backend<'_>) -> ServiceStatus {
    bridge.status()
}
#[tauri::command]
fn list_printers(bridge: Backend<'_>) -> Result<Vec<SharedPrinter>, String> {
    bridge.list_printers()
}
#[tauri::command]
fn save_printer(bridge: Backend<'_>, printer: SharedPrinter) -> Result<(), String> {
    bridge.save_printer(printer)
}
#[tauri::command]
async fn available_printers(bridge: Backend<'_>) -> Result<Vec<PrinterInfo>, String> {
    if bridge.status().simulation {
        return Ok(["模拟队列 A", "模拟队列 B"]
            .into_iter()
            .map(|name| PrinterInfo {
                name: name.into(),
                driver_name: "Simulation".into(),
                port_name: "none".into(),
            })
            .collect());
    }
    tauri::async_runtime::spawn_blocking(printing::enumerate_printers)
        .await
        .map_err(|_| "枚举任务异常退出".to_string())?
}
#[tauri::command]
fn list_devices(bridge: Backend<'_>) -> Result<Vec<Device>, String> {
    bridge.list_devices()
}
#[tauri::command]
fn create_device(bridge: Backend<'_>, name: String) -> Result<CreatedDevice, String> {
    bridge.create_device(name)
}
#[tauri::command]
fn revoke_device(bridge: Backend<'_>, id: String) -> Result<(), String> {
    bridge.revoke_device(&id)
}
#[tauri::command]
fn list_jobs(bridge: Backend<'_>) -> Result<Vec<Job>, String> {
    bridge.list_jobs()
}
#[tauri::command]
fn cancel_job(bridge: Backend<'_>, id: i64) -> Result<(), String> {
    bridge.cancel_job(id)
}
#[tauri::command]
fn start_service(
    bridge: Backend<'_>,
    manager: State<'_, Arc<SumatraManager>>,
    address: String,
) -> Result<(), String> {
    if !bridge.status().simulation && !manager.status().ready {
        return Err("请先到设置下载并校验 SumatraPDF 打印组件".into());
    }
    bridge.inner().start(&address)
}
#[tauri::command]
fn stop_service(bridge: Backend<'_>) -> Result<(), String> {
    bridge.stop()
}

#[tauri::command]
async fn sumatra_status(manager: State<'_, Arc<SumatraManager>>) -> Result<DownloadStatus, String> {
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || manager.status())
        .await
        .map_err(|_| "无法检查打印组件".to_string())
}

#[tauri::command]
async fn download_sumatra(
    manager: State<'_, Arc<SumatraManager>>,
) -> Result<DownloadStatus, String> {
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || manager.download())
        .await
        .map_err(|_| "打印组件下载任务异常，请重试".to_string())?
}

fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn stop_for_exit(bridge: &Bridge) -> Result<(), String> {
    bridge.stop()?;
    if bridge.has_active_print()? {
        return Err("正在等待当前打印调用结束。服务已停止接收新任务，请稍后再退出。".into());
    }
    Ok(())
}

fn tray_icon() -> Image<'static> {
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for y in 3..29 {
        for x in 3..29 {
            let i = (y * 32 + x) * 4;
            let paper = (8..24).contains(&x) && ((5..13).contains(&y) || (20..27).contains(&y));
            let color = if paper {
                [246, 244, 233, 255]
            } else {
                [31, 77, 58, 255]
            };
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }
    Image::new_owned(rgba, 32, 32)
}

pub fn run() {
    let simulation = std::env::args().any(|arg| arg == "--simulate");
    if !cfg!(windows) && !simulation {
        eprintln!("真实打印仅支持 Windows。在此平台启动桌面验证请使用 --simulate。");
        return;
    }
    let app = tauri::Builder::default()
        .setup(move |app| {
            let data = app.path().app_data_dir()?;
            let manager = Arc::new(SumatraManager::new(data.join("tools/sumatra")));
            let bridge = Bridge::open(&data, simulation, manager.exe_path())
                .map_err(std::io::Error::other)?;
            app.manage(bridge);
            app.manage(manager);
            let open = MenuItem::with_id(app, "open", "打开 Print Bridge", true, None::<&str>)?;
            let exit = MenuItem::with_id(app, "exit", "停止服务并退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &exit])?;
            TrayIconBuilder::new()
                .icon(tray_icon())
                .tooltip("Print Bridge")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show(app),
                    "exit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let _ = window.emit("hide-secret", ());
            }
        })
        .invoke_handler(tauri::generate_handler![
            service_status,
            list_printers,
            save_printer,
            available_printers,
            list_devices,
            create_device,
            revoke_device,
            list_jobs,
            cancel_job,
            start_service,
            stop_service,
            sumatra_status,
            download_sumatra
        ])
        .build(tauri::generate_context!())
        .expect("无法启动 Print Bridge 桌面应用");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if let Err(message) = stop_for_exit(app.state::<Arc<Bridge>>().inner()) {
                api.prevent_exit();
                show(app);
                let _ = app.emit("exit-blocked", message);
            }
        }
    });
}
