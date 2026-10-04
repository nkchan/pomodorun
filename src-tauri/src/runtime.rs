use crate::{
    clock,
    db::Database,
    model::*,
    slack::{Guard, SlackControl},
};
use serde_json::{json, Value};
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tauri_plugin_notification::NotificationExt;

pub struct Request {
    pub name: String,
    pub args: Value,
    pub reply: tokio::sync::oneshot::Sender<Result<Value>>,
}
pub struct Runtime {
    pub tx: mpsc::Sender<Request>,
    pub slack: Arc<SlackControl>,
}
impl Runtime {
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Request {
                name: name.into(),
                args,
                reply: tx,
            })
            .map_err(|_| AppError::new("worker", "アプリの処理が停止しました", true))?;
        rx.await
            .map_err(|_| AppError::new("worker", "応答を取得できませんでした", true))?
    }
}
pub fn launch(app: AppHandle, mut db: Database, slack: Arc<SlackControl>) -> Result<Runtime> {
    let (settings, mut timer) = db.load()?;
    if let Some(s) = timer.recover(&settings, clock::utc_ms()) {
        db.commit_timer(&timer, &[s])?;
    } else {
        timer.remaining_ms = settings.duration(Phase::Focus);
    }
    let warning = app
        .global_shortcut()
        .register(settings.shortcut.as_str())
        .err()
        .map(|_| {
            AppError::new(
                "shortcut",
                "保存済みショートカットを登録できません。設定から別のキーに変更してください。",
                false,
            )
        });
    let (tx, rx) = mpsc::channel::<Request>();
    let control = slack.clone();
    std::thread::Builder::new()
        .name("timer-storage".into())
        .spawn(move || {
            let mut w = Worker {
                app,
                db,
                timer,
                settings,
                selected: None,
                revision: 1,
                slack: control,
                last_tick: u64::MAX,
                warning,
            };
            loop {
                let req = rx.recv_timeout(Duration::from_millis(200));
                let advance = w.advance();
                if let Err(e) = &advance {
                    if clock::continuous_ms() / 1000 != w.last_tick {
                        let _ = w.app.emit("app:error", e);
                    }
                }
                match req {
                    Ok(req) => {
                        let result = advance.and_then(|_| w.execute(&req.name, req.args));
                        let _ = req.reply.send(result);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                w.present();
            }
        })
        .map_err(|_| AppError::new("worker", "ワーカーを開始できません", false))?;
    Ok(Runtime { tx, slack })
}
struct Worker {
    app: AppHandle,
    db: Database,
    timer: Timer,
    settings: Settings,
    selected: Option<String>,
    revision: u64,
    slack: Arc<SlackControl>,
    last_tick: u64,
    warning: Option<AppError>,
}
impl Worker {
    fn snapshot(&self) -> Snapshot {
        let mut timer = self.timer.clone();
        timer.remaining_ms = timer.remaining(clock::continuous_ms());
        Snapshot {
            timer,
            settings: self.settings.clone(),
            selected_task_id: self.selected.clone(),
            slack: self.slack.state(),
            features: LocalEntitlements.features(),
            revision: self.revision,
            warning: self.warning.clone(),
        }
    }
    fn changed(&mut self) {
        self.revision += 1;
        self.slack.set_desired(
            if self.settings.slack_enabled
                && self.timer.phase == Phase::Focus
                && self.timer.status == Status::Running
            {
                self.timer.session.as_ref().map(|s| Guard {
                    session_id: s.id.clone(),
                    generation: self.timer.generation,
                    end_utc: clock::utc_ms() + self.timer.remaining(clock::continuous_ms()) as i64,
                })
            } else {
                None
            },
        );
        let _ = self.app.emit(
            "app:changed",
            json!({"snapshot":self.snapshot(),"refresh":["tasks","brainDumps"]}),
        );
    }
    fn commit(&mut self, timer: Timer, done: &[Session]) -> Result<()> {
        self.db.commit_timer(&timer, done)?;
        self.timer = timer;
        self.changed();
        Ok(())
    }
    fn advance(&mut self) -> Result<()> {
        let mut next = self.timer.clone();
        let done = next.advance(&self.settings, clock::continuous_ms(), clock::utc_ms());
        if done.is_empty() {
            return Ok(());
        }
        self.commit(next, &done)?;
        for s in done {
            let _ = self.app.emit(
                "session:completed",
                json!({"session":s,"next":self.snapshot()}),
            );
            if self.settings.notifications {
                let body = if s.phase == Phase::Focus {
                    "集中が完了しました。休憩しましょう。"
                } else {
                    "休憩が完了しました。次の集中を開始できます。"
                };
                let _ = self
                    .app
                    .notification()
                    .builder()
                    .title("Pomodorun")
                    .body(body)
                    .show();
            }
            if self.settings.sound {
                std::thread::spawn(|| {
                    let _ = std::process::Command::new("/usr/bin/afplay")
                        .arg("/System/Library/Sounds/Glass.aiff")
                        .status();
                });
            }
        }
        Ok(())
    }
    fn present(&mut self) {
        let second = clock::continuous_ms() / 1000;
        if second == self.last_tick {
            return;
        }
        self.last_tick = second;
        let remaining = self.timer.remaining(clock::continuous_ms()).div_ceil(1000);
        if let Some(tray) = self.app.tray_by_id("main") {
            let title = if self.timer.status == Status::Idle {
                String::new()
            } else {
                format!("{:02}:{:02}", remaining / 60, remaining % 60)
            };
            let _ = tray.set_title(Some(title));
        }
        if self.timer.status == Status::Running {
            let _ = self.app.emit("timer:tick", json!({"sessionId":self.timer.session.as_ref().map(|s| &s.id),"revision":self.revision,"phase":self.timer.phase,"status":self.timer.status,"remainingMs":self.timer.remaining(clock::continuous_ms())}));
        }
    }
    fn execute(&mut self, name: &str, args: Value) -> Result<Value> {
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| AppError::invalid("入力が不足しています"))
        };
        let now = clock::utc_ms();
        let mono = clock::continuous_ms();
        match name {
            "get_app_state" => return Ok(serde_json::to_value(self.snapshot())?),
            "timer_start" => {
                let id = args.get("taskId").and_then(Value::as_str);
                let task = id.map(|id| self.db.task(id)).transpose()?;
                if task.as_ref().is_some_and(|t| t.completed_at.is_some()) {
                    return Err(AppError::invalid("完了したタスクは選択できません"));
                }
                let mut t = self.timer.clone();
                t.start(task.as_ref(), &self.settings, mono, now)?;
                self.db.commit_timer(&t, &[])?;
                self.timer = t;
                self.selected = id.map(str::to_owned);
                self.changed();
            }
            "timer_pause" | "timer_resume" | "timer_stop" | "timer_skip_break" => {
                let mut t = self.timer.clone();
                let mut done = Vec::new();
                match name {
                    "timer_pause" => t.pause(mono)?,
                    "timer_resume" => t.resume(mono, now)?,
                    "timer_skip_break" => {
                        if t.phase == Phase::Focus {
                            return Err(AppError::invalid("休憩中だけスキップできます"));
                        }
                        if let Some(s) = t.stop(&self.settings, mono, now) {
                            done.push(s);
                        }
                    }
                    _ => {
                        if let Some(s) = t.stop(&self.settings, mono, now) {
                            done.push(s);
                        }
                    }
                }
                self.commit(t, &done)?;
            }
            "tasks_list" => return Ok(serde_json::to_value(self.db.tasks(&text("date")?)?)?),
            "task_create" => {
                let estimate = args
                    .get("estimate")
                    .and_then(Value::as_u64)
                    .filter(|n| *n <= 99)
                    .ok_or_else(|| AppError::invalid("見積もりが不正です"))?
                    as u32;
                let id = self
                    .db
                    .create_task(&text("title")?, &text("date")?, estimate, now)?;
                self.changed();
                return Ok(json!(id));
            }
            "task_update" => {
                let mut task: Task = serde_json::from_value(
                    args.get("task")
                        .cloned()
                        .ok_or_else(|| AppError::invalid("タスクがありません"))?,
                )?;
                task.updated_at = now;
                self.db.update_task(&task)?;
                self.changed();
            }
            "task_archive" => {
                self.db.archive_task(&text("id")?, now)?;
                self.changed();
            }
            "brain_dump_add" => {
                let id = self.db.add_dump(
                    &text("text")?,
                    self.timer.session.as_ref().map(|s| s.id.as_str()),
                    now,
                )?;
                self.changed();
                return Ok(json!(id));
            }
            "brain_dump_list" => return Ok(serde_json::to_value(self.db.dumps()?)?),
            "brain_dump_convert" => {
                let id = self.db.convert_dump(&text("id")?, &text("date")?, now)?;
                self.changed();
                return Ok(json!(id));
            }
            "brain_dump_archive" => {
                self.db.archive_dump(&text("id")?, now)?;
                self.changed();
            }
            "settings_update" => {
                let settings: Settings = serde_json::from_value(
                    args.get("settings")
                        .cloned()
                        .ok_or_else(|| AppError::invalid("設定がありません"))?,
                )?;
                settings.validate()?;
                let different = settings.shortcut != self.settings.shortcut;
                if different {
                    self.app
                        .global_shortcut()
                        .register(settings.shortcut.as_str())
                        .map_err(|_| {
                            AppError::new(
                                "shortcut",
                                "ショートカットが不正、または他のアプリと競合しています",
                                false,
                            )
                        })?;
                }
                if let Err(e) = self.db.save_json("settings", &settings) {
                    if different {
                        let _ = self
                            .app
                            .global_shortcut()
                            .unregister(settings.shortcut.as_str());
                    }
                    return Err(e);
                }
                if different {
                    let _ = self
                        .app
                        .global_shortcut()
                        .unregister(self.settings.shortcut.as_str());
                    self.warning = None;
                }
                self.settings = settings;
                if self.timer.status == Status::Idle {
                    self.timer.remaining_ms = self.settings.duration(Phase::Focus);
                }
                self.changed();
            }
            _ => return Err(AppError::invalid("不明な操作です")),
        }
        Ok(serde_json::to_value(self.snapshot())?)
    }
}

pub fn show(app: &AppHandle, tab: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(Some(rect)) = tray.rect() {
            crate::position_popup(app, rect);
        }
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("ui:show", tab);
    }
}
