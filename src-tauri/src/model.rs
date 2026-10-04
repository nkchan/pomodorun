use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl AppError {
    pub fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
    pub fn invalid(message: &str) -> Self {
        Self::new("invalidInput", message, false)
    }
    pub fn storage() -> Self {
        Self::new(
            "storage",
            "保存に失敗しました。空き容量とアクセス権を確認してください。",
            true,
        )
    }
}
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for AppError {}
impl From<rusqlite::Error> for AppError {
    fn from(_: rusqlite::Error) -> Self {
        Self::storage()
    }
}
impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        Self::new("data", "保存データの形式を読み取れません。", false)
    }
}
pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub timer: Timer,
    pub settings: Settings,
    pub selected_task_id: Option<String>,
    pub slack: SlackState,
    pub features: serde_json::Value,
    pub revision: u64,
    pub warning: Option<AppError>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlackState {
    pub status: String,
    pub message: String,
    pub token_saved: bool,
    pub manual_restore_available: bool,
}
impl Default for SlackState {
    fn default() -> Self {
        Self {
            status: "disabled".into(),
            message: "Slack保護は未接続です".into(),
            token_saved: false,
            manual_restore_available: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub focus_minutes: u32,
    pub short_break_minutes: u32,
    pub long_break_minutes: u32,
    pub long_break_interval: u32,
    pub notifications: bool,
    pub sound: bool,
    pub shortcut: String,
    pub slack_enabled: bool,
    pub theme: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            focus_minutes: 25,
            short_break_minutes: 5,
            long_break_minutes: 15,
            long_break_interval: 4,
            notifications: true,
            sound: true,
            shortcut: "CommandOrControl+Shift+D".into(),
            slack_enabled: false,
            theme: "system".into(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if [
            self.focus_minutes,
            self.short_break_minutes,
            self.long_break_minutes,
        ]
        .iter()
        .any(|v| !(1..=180).contains(v))
            || !(1..=12).contains(&self.long_break_interval)
        {
            return Err(AppError::invalid(
                "時間は1〜180分、長休憩の間隔は1〜12回で指定してください。",
            ));
        }
        if !["system", "light", "dark"].contains(&self.theme.as_str())
            || self.shortcut.is_empty()
            || self.shortcut.len() > 100
        {
            return Err(AppError::invalid(
                "テーマまたはショートカットが正しくありません。",
            ));
        }
        Ok(())
    }
    pub fn duration(&self, phase: Phase) -> u64 {
        u64::from(match phase {
            Phase::Focus => self.focus_minutes,
            Phase::ShortBreak => self.short_break_minutes,
            Phase::LongBreak => self.long_break_minutes,
        }) * 60_000
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Focus,
    ShortBreak,
    LongBreak,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Idle,
    Running,
    Paused,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub task_id: Option<String>,
    pub task_title_snapshot: Option<String>,
    pub phase: Phase,
    pub planned_seconds: u64,
    pub elapsed_seconds: u64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timer {
    pub phase: Phase,
    pub status: Status,
    pub session: Option<Session>,
    pub remaining_ms: u64,
    pub deadline_utc: Option<i64>,
    pub completed_in_cycle: u32,
    pub generation: u64,
    #[serde(skip)]
    pub deadline_mono: Option<u64>,
}
impl Default for Timer {
    fn default() -> Self {
        Self {
            phase: Phase::Focus,
            status: Status::Idle,
            session: None,
            remaining_ms: 25 * 60_000,
            deadline_utc: None,
            deadline_mono: None,
            completed_in_cycle: 0,
            generation: 0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub title: String,
    pub scheduled_date: String,
    pub estimated_pomodoros: u32,
    pub completed_at: Option<i64>,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_pomodoros: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrainDump {
    pub id: String,
    pub text: String,
    pub captured_at: i64,
    pub session_id: Option<String>,
    pub converted_task_id: Option<String>,
    pub archived_at: Option<i64>,
}
pub fn validate_text(value: &str, max: usize) -> Result<String> {
    let s = value.trim();
    if s.is_empty() || s.chars().count() > max {
        Err(AppError::invalid(&format!(
            "1〜{max}文字で入力してください。"
        )))
    } else {
        Ok(s.into())
    }
}
pub fn validate_date(value: &str) -> Result<()> {
    if value.len() != 10 || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() {
        Err(AppError::invalid("日付が正しくありません。"))
    } else {
        Ok(())
    }
}
pub trait EntitlementProvider {
    fn features(&self) -> serde_json::Value;
}
pub struct LocalEntitlements;
impl EntitlementProvider for LocalEntitlements {
    fn features(&self) -> serde_json::Value {
        serde_json::json!({"focusGuard": true, "analytics": false, "export": false, "tier": "free", "source": "mvp"})
    }
}
