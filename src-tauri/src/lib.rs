pub mod domain;
pub mod platform;
pub mod services;

#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use services::panel::PanelController;

#[cfg(windows)]
use tauri::Manager;

#[cfg(test)]
mod tests;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg(windows)]
#[tauri::command]
fn show_quick_panel(controller: tauri::State<'_, Arc<PanelController>>) -> Result<(), String> {
    controller.show().map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn hide_quick_panel(controller: tauri::State<'_, Arc<PanelController>>) -> Result<(), String> {
    controller.hide().map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn toggle_quick_panel(controller: tauri::State<'_, Arc<PanelController>>) -> Result<(), String> {
    controller.toggle().map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    if let Err(error) = platform::windows::enable_per_monitor_v2() {
        eprintln!(
            "Per-Monitor V2 DPI awareness could not be set before window creation; continuing with the process DPI mode: {error}"
        );
    }

    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder
        .setup(|app| {
            let panel = app
                .get_webview_window("quick-panel")
                .ok_or_else(|| "quick-panel window is missing".to_owned())?;
            platform::windows::configure_quick_panel_style(panel.hwnd()?)?;
            app.manage(Arc::new(PanelController::new(panel)));
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "quick-panel" {
                return;
            }
            let controller = window.state::<Arc<PanelController>>();
            match event {
                tauri::WindowEvent::Focused(focused) => {
                    let _ = controller.on_focus_changed(*focused);
                }
                tauri::WindowEvent::ScaleFactorChanged { .. } => {
                    let _ = controller.reposition_if_visible();
                }
                tauri::WindowEvent::Destroyed => {
                    let _ = controller.hide();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            show_quick_panel,
            hide_quick_panel,
            toggle_quick_panel
        ]);
    #[cfg(not(windows))]
    let builder = builder.invoke_handler(tauri::generate_handler![greet]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
