mod clock;
mod db;
mod model;
mod runtime;
mod slack;
mod timer;

use model::{Result, Status};
use runtime::Runtime;
use serde_json::{json, Value};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

macro_rules! command {
    ($($name:ident),*) => {$ (
        #[tauri::command]
        async fn $name(state: tauri::State<'_, Runtime>, args: Option<Value>) -> Result<Value> {
            state.call(stringify!($name), args.unwrap_or(json!({}))).await
        }
    )*};
}
command!(
    get_app_state,
    timer_start,
    timer_pause,
    timer_resume,
    timer_stop,
    timer_skip_break,
    tasks_list,
    task_create,
    task_update,
    task_archive,
    brain_dump_add,
    brain_dump_list,
    brain_dump_convert,
    brain_dump_archive,
    settings_update
);
macro_rules! slack_command {
    ($($name:ident),*) => {$ (
        #[tauri::command]
        async fn $name(state: tauri::State<'_, Runtime>, args: Option<Value>) -> Result<Value> {
            let token = args.and_then(|a| a.get("token").and_then(Value::as_str).map(str::to_owned));
            state.slack.call(stringify!($name), token).await
        }
    )*};
}
slack_command!(
    slack_save_token,
    slack_test_connection,
    slack_disconnect,
    slack_retry_restore
);

fn position_popup(app: &tauri::AppHandle, rect: tauri::Rect) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    let point = rect.position.to_physical::<f64>(scale);
    let rect_size = rect.size.to_physical::<f64>(scale);
    if let Ok(Some(monitor)) = app.monitor_from_point(point.x, point.y) {
        let area = monitor.work_area();
        let size = w.outer_size().unwrap_or(tauri::PhysicalSize::new(
            (360.0 * scale) as u32,
            (620.0 * scale) as u32,
        ));
        let x = (point.x + rect_size.width / 2.0 - size.width as f64 / 2.0).clamp(
            area.position.x as f64,
            (area.position.x as f64 + area.size.width as f64 - size.width as f64)
                .max(area.position.x as f64),
        );
        let y = (point.y + rect_size.height + 4.0).clamp(
            area.position.y as f64,
            (area.position.y as f64 + area.size.height as f64 - size.height as f64)
                .max(area.position.y as f64),
        );
        let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
    }
}
fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            runtime::show(app, "timer")
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        if let Some(tray) = app.tray_by_id("main") {
                            if let Ok(Some(rect)) = tray.rect() {
                                position_popup(app, rect);
                            }
                        }
                        runtime::show(app, "dump");
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            timer_start,
            timer_pause,
            timer_resume,
            timer_stop,
            timer_skip_break,
            tasks_list,
            task_create,
            task_update,
            task_archive,
            brain_dump_add,
            brain_dump_list,
            brain_dump_convert,
            brain_dump_archive,
            settings_update,
            slack_save_token,
            slack_test_connection,
            slack_disconnect,
            slack_retry_restore
        ])
        .setup(|app| {
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("pomodorun.sqlite3");
            let db = db::Database::open(&path)?;
            let slack = slack::launch(app.handle().clone(), path)?;
            let runtime = runtime::launch(app.handle().clone(), db, slack)?;
            app.manage(runtime);
            let start = MenuItem::with_id(app, "toggle", "開始／停止", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "設定", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "終了", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&start, &settings, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/tray.png"
                ))?)
                .icon_as_template(true)
                .tooltip("Pomodorun")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "quit" => app.exit(0),
                    "settings" => runtime::show(app, "settings"),
                    "toggle" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<Runtime>();
                            if let Ok(s) = state.call("get_app_state", json!({})).await {
                                let name = if s["timer"]["status"]
                                    == serde_json::to_value(Status::Idle).unwrap()
                                {
                                    "timer_start"
                                } else {
                                    "timer_stop"
                                };
                                if state.call(name, json!({})).await.is_err() {
                                    runtime::show(&app, "timer");
                                }
                            }
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                position_popup(app, rect);
                                runtime::show(app, "timer");
                            }
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|w, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = w.hide();
            }
            tauri::WindowEvent::Focused(false) => {
                // Let the tray click toggle the still-visible window before focus-loss dismissal.
                let w = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(120));
                    if !w.is_focused().unwrap_or(false) {
                        let _ = w.hide();
                    }
                });
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("Pomodorunを起動できませんでした");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            if let Some(r) = app.try_state::<Runtime>() {
                r.slack.shutdown();
            }
        }
    });
}
