use crate::{clock, db::Database, model::*};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter};

const SERVICE: &str = "dev.pomodorun.app.slack";
const ACCOUNT: &str = "user-token";
fn credential() -> Result<Option<String>> {
    use security_framework::passwords::get_generic_password;
    match get_generic_password(SERVICE, ACCOUNT) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| AppError::new("keychain", "Keychainの資格情報を読み取れません", false)),
        Err(e) if e.code() == -25300 => Ok(None),
        Err(_) => Err(AppError::new(
            "keychain",
            "Keychainへのアクセスに失敗しました",
            true,
        )),
    }
}
fn save_credential(token: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, token.as_bytes())
        .map_err(|_| AppError::new("keychain", "Keychainへの保存に失敗しました", true))
}
fn delete_credential() -> Result<()> {
    security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT)
        .map_err(|_| AppError::new("keychain", "Keychainから削除できませんでした", true))
}
#[derive(Clone, Debug)]
pub struct Guard {
    pub session_id: String,
    pub generation: u64,
    pub end_utc: i64,
}
impl PartialEq for Guard {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id && self.generation == other.generation
    }
}
type Reply = tokio::sync::oneshot::Sender<Result<Value>>;
struct Operation {
    name: String,
    token: Option<String>,
    reply: Reply,
}
pub struct SlackControl {
    desired: Mutex<Option<Guard>>,
    state: Mutex<SlackState>,
    tx: mpsc::Sender<Operation>,
}
impl SlackControl {
    pub fn state(&self) -> SlackState {
        self.state.lock().unwrap().clone()
    }
    pub fn set_desired(&self, guard: Option<Guard>) {
        *self.desired.lock().unwrap() = guard;
    }
    fn desired(&self) -> Option<Guard> {
        self.desired.lock().unwrap().clone()
    }
    pub async fn call(&self, name: &str, token: Option<String>) -> Result<Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Operation {
                name: name.into(),
                token,
                reply: tx,
            })
            .map_err(|_| AppError::new("slack", "Slackワーカーが停止しました", true))?;
        rx.await
            .map_err(|_| AppError::new("slack", "Slackの応答を取得できません", true))?
    }
    pub fn shutdown(&self) {
        self.set_desired(None);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(Operation {
            name: "shutdown".into(),
            token: None,
            reply: tx,
        });
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if rx.try_recv().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Profile {
    status_text: String,
    status_emoji: String,
    status_expiration: i64,
}
impl Profile {
    fn parse(v: &Value) -> Self {
        Self {
            status_text: v["status_text"].as_str().unwrap_or_default().into(),
            status_emoji: v["status_emoji"].as_str().unwrap_or_default().into(),
            status_expiration: v["status_expiration"].as_i64().unwrap_or_default(),
        }
    }
    fn restored(&self, now: i64) -> Self {
        if self.status_expiration > 0 && self.status_expiration <= now {
            Self::default()
        } else {
            self.clone()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    session_id: String,
    generation: u64,
    original: Profile,
    applied: Profile,
    original_snooze_end: i64,
    applied_snooze_end: i64,
    profile_pending: bool,
    snooze_pending: bool,
    manual: bool,
    attempts: u32,
    next_attempt: i64,
    error_code: Option<String>,
}
struct Api {
    client: Client,
    base: String,
    token: String,
}
#[derive(Debug)]
struct Failure {
    error: AppError,
    wait: Option<u64>,
}
impl From<AppError> for Failure {
    fn from(error: AppError) -> Self {
        Self { error, wait: None }
    }
}
type ApiResult<T> = std::result::Result<T, Failure>;
impl Api {
    fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| {
                    AppError::new("network", "HTTPクライアントを初期化できません", true)
                })?,
            base: "https://slack.com/api/".into(),
            token,
        })
    }
    fn call(&self, method: &str, body: Value) -> ApiResult<(Value, Vec<String>)> {
        let response = self
            .client
            .post(format!("{}{method}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(|_| {
                Failure::from(AppError::new(
                    "network",
                    "Slackに接続できません。自動で再試行します",
                    true,
                ))
            })?;
        let scopes = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .collect();
        if response.status().as_u16() == 429 {
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            return Err(Failure {
                error: AppError::new(
                    "rateLimited",
                    "Slackの利用制限中です。指定時間後に再試行します",
                    true,
                ),
                wait: Some(wait),
            });
        }
        if !response.status().is_success() {
            return Err(AppError::new(
                "network",
                "Slackの通信エラーです",
                response.status().is_server_error(),
            )
            .into());
        }
        let v: Value = response.json().map_err(|_| {
            Failure::from(AppError::new(
                "network",
                "Slackの応答を読み取れません",
                true,
            ))
        })?;
        if v["ok"] != true {
            let err = match v["error"].as_str().unwrap_or_default() {
                "invalid_auth" | "token_revoked" | "token_expired" | "account_inactive"
                | "not_authed" => AppError::new(
                    "needsAuth",
                    "Slack認証が無効です。トークンを確認してください",
                    false,
                ),
                "missing_scope" | "not_allowed_token_type" | "no_permission" => AppError::new(
                    "needsAuth",
                    "Slackのユーザートークンと必要権限を確認してください",
                    false,
                ),
                "ratelimited" => AppError::new("rateLimited", "Slackの利用制限中です", true),
                "internal_error" | "fatal_error" | "service_unavailable" | "request_timeout" => {
                    AppError::new("network", "Slackの一時的な障害です。再試行します", true)
                }
                _ => AppError::new("slackApi", "Slackの操作を完了できませんでした", false),
            };
            return Err(err.into());
        }
        Ok((v, scopes))
    }
    fn profile(&self) -> ApiResult<Profile> {
        Ok(Profile::parse(
            &self.call("users.profile.get", json!({}))?.0["profile"],
        ))
    }
    fn snooze(&self) -> ApiResult<i64> {
        let v = self.call("dnd.info", json!({}))?.0;
        Ok(if v["snooze_enabled"] == true {
            if v["snooze_is_indefinite"] == true {
                i64::MAX
            } else {
                v["snooze_endtime"].as_i64().unwrap_or_default()
            }
        } else {
            0
        })
    }
    fn set_profile(&self, profile: &Profile) -> ApiResult<()> {
        self.call("users.profile.set", json!({"profile":profile}))?;
        Ok(())
    }
    fn set_snooze(&self, minutes: i64) -> ApiResult<i64> {
        let v = self
            .call("dnd.setSnooze", json!({"num_minutes":minutes}))?
            .0;
        Ok(v["snooze_endtime"].as_i64().unwrap_or_default())
    }
    fn test(&self) -> ApiResult<Value> {
        let (auth, mut scopes) = self.call("auth.test", json!({}))?;
        scopes.extend(self.call("users.profile.get", json!({}))?.1);
        scopes.extend(self.call("dnd.info", json!({}))?.1);
        if [
            "users.profile:read",
            "users.profile:write",
            "dnd:read",
            "dnd:write",
        ]
        .iter()
        .any(|s| !scopes.iter().any(|v| v == s))
        {
            return Err(AppError::new(
                "needsAuth",
                "必要権限: users.profile:read/write と dnd:read/write を追加してください",
                false,
            )
            .into());
        }
        Ok(json!({"teamId":auth["team_id"],"userId":auth["user_id"]}))
    }
}
pub fn launch(app: AppHandle, path: PathBuf) -> Result<Arc<SlackControl>> {
    let db = Database::open(&path)?;
    let journal = db.load_json::<Journal>("slack_guard_journal")?;
    let (token, credential_error) = match credential() {
        Ok(token) => (token, None),
        Err(e) => (None, Some(e)),
    };
    let (tx, rx) = mpsc::channel();
    let control = Arc::new(SlackControl {
        desired: Mutex::new(None),
        state: Mutex::new(SlackState {
            token_saved: token.is_some(),
            status: if credential_error.is_some() {
                "needsAuth".into()
            } else {
                "disabled".into()
            },
            message: credential_error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "Slack保護は未接続です".into()),
            manual_restore_available: false,
        }),
        tx,
    });
    let handle = control.clone();
    std::thread::Builder::new()
        .name("slack-guard".into())
        .spawn(move || {
            let mut w = SlackWorker {
                db,
                api: token.and_then(|t| Api::new(t).ok()),
                journal,
                app: Some(app),
                control: handle,
                active: None,
                blocked: credential_error.is_some(),
                retry_at: 0,
                failures: 0,
            };
            loop {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(op) => {
                        let shutdown = op.name == "shutdown";
                        let result = w.operation(&op.name, op.token);
                        let _ = op.reply.send(result);
                        if shutdown {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                w.step();
            }
        })
        .map_err(|_| AppError::new("worker", "Slackワーカーを開始できません", false))?;
    Ok(control)
}
struct SlackWorker {
    db: Database,
    api: Option<Api>,
    journal: Option<Journal>,
    app: Option<AppHandle>,
    control: Arc<SlackControl>,
    active: Option<Guard>,
    blocked: bool,
    retry_at: i64,
    failures: u32,
}
trait FocusGuard {
    fn protect(&mut self, guard: &Guard) -> ApiResult<()>;
    fn restore(&mut self, explicit: bool) -> ApiResult<()>;
}
impl FocusGuard for SlackWorker {
    fn protect(&mut self, guard: &Guard) -> ApiResult<()> {
        self.apply(guard)
    }
    fn restore(&mut self, explicit: bool) -> ApiResult<()> {
        self.restore_existing(explicit)
    }
}
impl SlackWorker {
    fn state(&self, status: &str, message: &str, manual: bool) {
        let next = SlackState {
            status: status.into(),
            message: message.into(),
            token_saved: self.api.is_some(),
            manual_restore_available: manual,
        };
        let mut state = self.control.state.lock().unwrap();
        if state.status != next.status
            || state.message != next.message
            || state.token_saved != next.token_saved
            || state.manual_restore_available != manual
        {
            *state = next.clone();
            if let Some(app) = &self.app {
                let _ = app.emit("slack:changed", next);
            }
        }
    }
    fn persist(&self) -> Result<()> {
        if let Some(j) = &self.journal {
            self.db.save_json("slack_guard_journal", j)
        } else {
            self.db
                .conn
                .execute("DELETE FROM slack_guard_journal", [])?;
            Ok(())
        }
    }
    fn fail(&mut self, f: Failure) {
        self.blocked = !f.error.retryable;
        self.failures = self.failures.saturating_add(1);
        let seconds = f
            .wait
            .unwrap_or_else(|| {
                (1u64 << self.failures.min(5)) + (clock::continuous_ms() % 1000) / 250
            })
            .min(if f.wait.is_some() { u64::MAX } else { 60 });
        self.retry_at = clock::utc_ms()
            .saturating_add(seconds.saturating_mul(1000).min(i64::MAX as u64) as i64);
        if let Some(j) = &mut self.journal {
            j.attempts = self.failures;
            j.next_attempt = self.retry_at;
            j.error_code = Some(f.error.code.clone());
            let _ = self.persist();
        }
        self.state(
            if self.blocked {
                "needsAuth"
            } else {
                "degraded"
            },
            &f.error.message,
            false,
        );
    }
    fn step(&mut self) {
        if self.blocked || clock::utc_ms() < self.retry_at {
            return;
        }
        let desired = self.control.desired();
        if let Some(j) = &self.journal {
            let same = desired
                .as_ref()
                .is_some_and(|g| g.session_id == j.session_id && g.generation == j.generation)
                && self.active == desired;
            if same {
                self.state("protecting", "Slackステータスと通知を保護中", false);
                return;
            }
            if j.manual && !j.snooze_pending {
                self.state(
                    "degraded",
                    "期限切れ・手動クリアを区別できません。元ステータスの明示復元が必要です",
                    true,
                );
                return;
            }
            self.state("restoring", "Slackの元の状態を復元中", false);
            if let Err(f) = self.restore(false) {
                self.fail(f);
                return;
            }
            if self.journal.is_some() {
                return;
            }
            self.active = None;
        }
        if let Some(g) = desired {
            if self.api.is_none() {
                self.state("disabled", "Slack未接続のため保護は適用されません", false);
                return;
            }
            self.state("connecting", "Slack保護を適用中", false);
            if let Err(f) = self.protect(&g) {
                // Even a nonretryable profile failure may follow a successful DND write.
                if !f.error.retryable && self.journal.is_some() {
                    let _ = self.restore(false);
                }
                self.fail(f);
            }
        } else {
            self.state(
                "disabled",
                if self.api.is_some() {
                    "Slack保護は待機中"
                } else {
                    "Slack保護は未接続です"
                },
                false,
            );
        }
    }
    fn apply(&mut self, g: &Guard) -> ApiResult<()> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| AppError::new("needsAuth", "Slackに接続してください", false))?;
        let original = api.profile()?;
        let original_end = api.snooze()?;
        if self.control.desired().as_ref() != Some(g) || g.end_utc <= clock::utc_ms() {
            return Ok(());
        }
        let end = g.end_utc / 1000;
        let local = chrono::DateTime::from_timestamp(end, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let applied = Profile {
            status_text: format!("集中中 (~{local}まで)"),
            status_emoji: ":tomato:".into(),
            status_expiration: end,
        };
        self.journal = Some(Journal {
            session_id: g.session_id.clone(),
            generation: g.generation,
            original,
            applied,
            original_snooze_end: original_end,
            applied_snooze_end: end,
            profile_pending: false,
            snooze_pending: false,
            manual: false,
            attempts: 0,
            next_attempt: 0,
            error_code: None,
        });
        self.persist()?;
        if original_end < end {
            self.journal.as_mut().unwrap().snooze_pending = true;
            self.persist()?;
            let actual = api.set_snooze(((end - clock::utc_ms() / 1000).max(1) + 59) / 60)?;
            self.journal.as_mut().unwrap().applied_snooze_end = actual;
            self.persist()?;
        }
        if self.control.desired().as_ref() != Some(g) {
            return Ok(());
        }
        self.journal.as_mut().unwrap().profile_pending = true;
        self.persist()?;
        api.set_profile(&self.journal.as_ref().unwrap().applied)?;
        self.active = Some(g.clone());
        self.failures = 0;
        Ok(())
    }
    fn restore_existing(&mut self, force: bool) -> ApiResult<()> {
        let Some(mut j) = self.journal.clone() else {
            return Ok(());
        };
        let api = self.api.as_ref().ok_or_else(|| {
            AppError::new(
                "needsAuth",
                "復元に必要なSlackトークンを設定してください",
                false,
            )
        })?;
        let now = clock::utc_ms() / 1000;
        let mut failure = None;
        // Each independent component is re-read immediately before conditional restoration.
        if j.snooze_pending {
            let result = (|| -> ApiResult<()> {
                let current = api.snooze()?;
                if snooze_matches(current, j.applied_snooze_end, now) {
                    if j.original_snooze_end > now {
                        api.set_snooze((j.original_snooze_end - now + 59) / 60)?;
                    } else {
                        api.call("dnd.endSnooze", json!({}))?;
                    }
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    j.snooze_pending = false;
                    self.journal = Some(j.clone());
                    self.persist()?;
                }
                Err(f) => failure = Some(f),
            }
        }
        if j.profile_pending && (!j.manual || force) {
            let result = (|| -> ApiResult<bool> {
                let current = api.profile()?;
                if current == j.applied || (force && j.manual) {
                    api.set_profile(&j.original.restored(now))?;
                } else if current == Profile::default() && j.applied.status_expiration <= now {
                    return Ok(true);
                }
                Ok(false)
            })();
            match result {
                Ok(manual) => {
                    j.manual = manual;
                    j.profile_pending = manual;
                    self.journal = Some(j.clone());
                    self.persist()?;
                }
                Err(f) => {
                    if failure.is_none() {
                        failure = Some(f);
                    }
                }
            }
        }
        if let Some(f) = failure {
            return Err(f);
        }
        if j.manual {
            self.state(
                "degraded",
                "元ステータスを戻すには明示復元を実行してください",
                true,
            );
            return Ok(());
        }
        self.journal = None;
        if let Err(e) = self.persist() {
            self.journal = Some(j);
            return Err(e.into());
        }
        self.active = None;
        self.retry_at = 0;
        Ok(())
    }
    fn operation(&mut self, name: &str, token: Option<String>) -> Result<Value> {
        match name {
            "slack_retry_restore" => {
                if self.api.is_none() {
                    self.api = credential()?.map(Api::new).transpose()?;
                }
                self.blocked = false;
                self.retry_at = 0;
                self.restore(true).map_err(|f| {
                    let error = f.error.clone();
                    self.fail(f);
                    error
                })?;
            }
            "slack_save_token" | "slack_disconnect" => {
                if self.journal.is_some() {
                    self.restore(false).map_err(|f| {
                        let e = f.error.clone();
                        self.fail(f);
                        e
                    })?;
                    if self.journal.is_some() {
                        return Err(AppError::new(
                            "restorePending",
                            "元ステータスを明示復元してから接続を変更してください",
                            false,
                        ));
                    }
                }
                if name == "slack_disconnect" {
                    if self.api.is_some() {
                        delete_credential()?;
                    }
                    self.api = None;
                    self.db.conn.execute("DELETE FROM slack_connection", [])?;
                } else {
                    let token =
                        token.ok_or_else(|| AppError::invalid("トークンを入力してください"))?;
                    if !token.starts_with("xoxp-")
                        || token.len() > 1024
                        || token.chars().any(char::is_whitespace)
                    {
                        return Err(AppError::invalid("xoxp-ユーザートークンを入力してください"));
                    }
                    let api = Api::new(token.clone())?;
                    save_credential(&token)?;
                    self.api = Some(api);
                    self.db.conn.execute("DELETE FROM slack_connection", [])?;
                }
                self.blocked = false;
                self.retry_at = 0;
                self.active = None;
            }
            "slack_test_connection" => {
                let api = self.api.as_ref().ok_or_else(|| {
                    AppError::new("needsAuth", "先にトークンを保存してください", false)
                })?;
                let auth = api.test().map_err(|f| {
                    let e = f.error.clone();
                    self.fail(f);
                    e
                })?;
                self.db.conn.execute("INSERT INTO slack_connection VALUES(1,?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET team_id=excluded.team_id,user_id=excluded.user_id,credential_ref=excluded.credential_ref,last_verified_at=excluded.last_verified_at", rusqlite::params![auth["teamId"].as_str().unwrap_or_default(), auth["userId"].as_str().unwrap_or_default(), SERVICE, clock::utc_ms()])?;
                self.blocked = false;
                self.retry_at = 0;
                return Ok(
                    json!({"message":"接続と必要権限を確認しました（書き込みは行っていません）"}),
                );
            }
            "shutdown" => {
                self.control.set_desired(None);
                self.restore(false).map_err(|f| f.error)?;
            }
            _ => return Err(AppError::invalid("不明なSlack操作です")),
        }
        self.state("disabled", "Slack保護は待機中", false);
        Ok(serde_json::to_value(self.control.state())?)
    }
}
fn snooze_matches(current: i64, applied: i64, now: i64) -> bool {
    current > now && (current - applied).abs() <= 60
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    fn mock(status: &str, headers: &str, body: &str) -> Api {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = s.read(&mut buf);
            s.write_all(response.as_bytes()).unwrap();
        });
        let mut api = Api::new("mock-only".into()).unwrap();
        api.base = base;
        api
    }
    #[test]
    fn http_ok_false_never_leaks_response() {
        let err = mock(
            "200 OK",
            "",
            r#"{"ok":false,"error":"invalid_auth","secret":"never echo"}"#,
        )
        .profile()
        .unwrap_err();
        assert_eq!(err.error.code, "needsAuth");
        assert!(!err.error.message.contains("never echo"));
        assert!(!err.error.retryable);
    }
    #[test]
    fn rate_limit_honors_retry_after() {
        let err = mock("429 Too Many Requests", "Retry-After: 123\r\n", "{}")
            .profile()
            .unwrap_err();
        assert_eq!(err.wait, Some(123));
    }
    #[test]
    fn offline_and_server_failures_retry() {
        assert!(
            mock("503 Unavailable", "", "{}")
                .profile()
                .unwrap_err()
                .error
                .retryable
        );
        let mut api = Api::new("mock-only".into()).unwrap();
        api.base = "http://127.0.0.1:1/".into();
        assert!(api.profile().unwrap_err().error.retryable);
    }
    #[test]
    fn expired_original_is_cleared_and_manual_snooze_kept() {
        let p = Profile {
            status_text: "old".into(),
            status_emoji: ":wave:".into(),
            status_expiration: 20,
        };
        assert_eq!(p.restored(21), Profile::default());
        assert!(!snooze_matches(1000, 500, 100));
        assert!(snooze_matches(530, 500, 100));
        assert!(!snooze_matches(0, 500, 600));
    }

    fn worker() -> SlackWorker {
        let (tx, _) = mpsc::channel();
        SlackWorker {
            db: Database::open(std::path::Path::new(":memory:")).unwrap(),
            api: None,
            journal: None,
            app: None,
            control: Arc::new(SlackControl {
                tx,
                desired: Mutex::new(None),
                state: Mutex::new(SlackState::default()),
            }),
            active: None,
            blocked: false,
            retry_at: 0,
            failures: 0,
        }
    }
    fn journal() -> Journal {
        let now = clock::utc_ms() / 1000;
        Journal {
            session_id: "session".into(),
            generation: 1,
            original: Profile {
                status_text: "original".into(),
                status_emoji: ":wave:".into(),
                status_expiration: 0,
            },
            applied: Profile {
                status_text: "focus".into(),
                status_emoji: ":tomato:".into(),
                status_expiration: now + 600,
            },
            original_snooze_end: 0,
            applied_snooze_end: now + 600,
            profile_pending: true,
            snooze_pending: true,
            manual: false,
            attempts: 0,
            next_attempt: 0,
            error_code: None,
        }
    }
    fn sequence(
        steps: Vec<(&'static str, Value)>,
        stopped: Option<Arc<SlackControl>>,
    ) -> (Api, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (method, body) in steps {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..pos]);
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= pos + 4 + length {
                            break;
                        }
                    }
                }
                assert!(String::from_utf8_lossy(&request).starts_with(&format!("POST /{method} ")));
                if method == "dnd.setSnooze" {
                    if let Some(control) = &stopped {
                        control.set_desired(None);
                    }
                }
                let body = body.to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-oauth-scopes: users.profile:read,users.profile:write,dnd:read,dnd:write\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let mut api = Api::new("mock-only".into()).unwrap();
        api.base = base;
        (api, server)
    }
    #[test]
    fn crash_journal_restores_both_components_and_clears_only_after_success() {
        let mut w = worker();
        let j = journal();
        w.journal = Some(j.clone());
        w.persist().unwrap();
        w.journal = w.db.load_json("slack_guard_journal").unwrap();
        let (api, server) = sequence(
            vec![
                (
                    "dnd.info",
                    json!({"ok":true,"snooze_enabled":true,"snooze_endtime":j.applied_snooze_end}),
                ),
                ("dnd.endSnooze", json!({"ok":true})),
                ("users.profile.get", json!({"ok":true,"profile":j.applied})),
                ("users.profile.set", json!({"ok":true})),
            ],
            None,
        );
        w.api = Some(api);
        w.restore(false).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
        assert!(w
            .db
            .load_json::<Journal>("slack_guard_journal")
            .unwrap()
            .is_none());
    }
    #[test]
    fn manual_profile_and_snooze_are_not_overwritten_even_by_normal_retry() {
        let mut w = worker();
        let j = journal();
        w.journal = Some(j.clone());
        let (api, server) = sequence(
            vec![
                (
                    "dnd.info",
                    json!({"ok":true,"snooze_enabled":true,"snooze_endtime":j.applied_snooze_end+1000}),
                ),
                (
                    "users.profile.get",
                    json!({"ok":true,"profile":{"status_text":"manual","status_emoji":":sunny:","status_expiration":0}}),
                ),
            ],
            None,
        );
        w.api = Some(api);
        w.restore(true).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
    }
    #[test]
    fn expiry_requires_explicit_restore_and_blocks_new_application() {
        let mut w = worker();
        let mut j = journal();
        j.applied.status_expiration = clock::utc_ms() / 1000 - 1;
        j.snooze_pending = false;
        w.journal = Some(j);
        let (api, server) = sequence(
            vec![
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("users.profile.set", json!({"ok":true})),
            ],
            None,
        );
        w.api = Some(api);
        w.restore(false).unwrap();
        assert!(w.journal.as_ref().unwrap().manual);
        w.control.set_desired(Some(Guard {
            session_id: "new".into(),
            generation: 2,
            end_utc: clock::utc_ms() + 60000,
        }));
        w.step();
        assert!(w.control.state().manual_restore_available);
        w.restore(true).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
    }
    #[test]
    fn partial_restoration_saves_success_and_retries_failed_component_only() {
        let mut w = worker();
        let j = journal();
        w.journal = Some(j.clone());
        let (api, server) = sequence(
            vec![
                ("dnd.info", json!({"ok":false,"error":"internal_error"})),
                ("users.profile.get", json!({"ok":true,"profile":j.applied})),
                ("users.profile.set", json!({"ok":true})),
                ("dnd.info", json!({"ok":true,"snooze_enabled":false})),
            ],
            None,
        );
        w.api = Some(api);
        assert!(w.restore(false).is_err());
        let persisted =
            w.db.load_json::<Journal>("slack_guard_journal")
                .unwrap()
                .unwrap();
        assert!(!persisted.profile_pending);
        assert!(persisted.snooze_pending);
        w.restore(false).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
    }
    #[test]
    fn stop_during_snooze_response_invalidates_later_profile_write() {
        let mut w = worker();
        let g = Guard {
            session_id: "s".into(),
            generation: 3,
            end_utc: clock::utc_ms() + 600000,
        };
        w.control.set_desired(Some(g.clone()));
        let end = clock::utc_ms() / 1000 + 600;
        let (api, server) = sequence(
            vec![
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("dnd.info", json!({"ok":true,"snooze_enabled":false})),
                ("dnd.setSnooze", json!({"ok":true,"snooze_endtime":end})),
                (
                    "dnd.info",
                    json!({"ok":true,"snooze_enabled":true,"snooze_endtime":end}),
                ),
                ("dnd.endSnooze", json!({"ok":true})),
            ],
            Some(w.control.clone()),
        );
        w.api = Some(api);
        w.protect(&g).unwrap();
        assert!(!w.journal.as_ref().unwrap().profile_pending);
        w.restore(false).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
    }
    #[test]
    fn existing_longer_snooze_is_never_changed() {
        let mut w = worker();
        let g = Guard {
            session_id: "s".into(),
            generation: 1,
            end_utc: clock::utc_ms() + 60000,
        };
        w.control.set_desired(Some(g.clone()));
        let (api, server) = sequence(
            vec![
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                (
                    "dnd.info",
                    json!({"ok":true,"snooze_enabled":true,"snooze_endtime":clock::utc_ms()/1000+3600}),
                ),
                ("users.profile.set", json!({"ok":true})),
            ],
            None,
        );
        w.api = Some(api);
        w.protect(&g).unwrap();
        server.join().unwrap();
        assert!(!w.journal.as_ref().unwrap().snooze_pending);
    }
    #[test]
    fn failed_journal_write_prevents_any_slack_changes() {
        let mut w = worker();
        let g = Guard {
            session_id: "s".into(),
            generation: 1,
            end_utc: clock::utc_ms() + 60000,
        };
        w.control.set_desired(Some(g.clone()));
        w.db.conn.execute_batch("CREATE TRIGGER fail BEFORE INSERT ON slack_guard_journal BEGIN SELECT RAISE(ABORT,'full'); END;").unwrap();
        let (api, server) = sequence(
            vec![
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("dnd.info", json!({"ok":true,"snooze_enabled":false})),
            ],
            None,
        );
        w.api = Some(api);
        assert_eq!(w.protect(&g).unwrap_err().error.code, "storage");
        server.join().unwrap();
        assert!(!w.journal.as_ref().unwrap().snooze_pending);
        assert!(!w.journal.as_ref().unwrap().profile_pending);
    }

    #[test]
    fn connection_test_uses_read_methods_only_and_checks_scopes() {
        let (api, server) = sequence(
            vec![
                ("auth.test", json!({"ok":true,"team_id":"T","user_id":"U"})),
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("dnd.info", json!({"ok":true})),
            ],
            None,
        );
        assert_eq!(api.test().unwrap()["userId"], "U");
        server.join().unwrap();
        assert_eq!(
            mock("200 OK", "", r#"{"ok":false,"error":"missing_scope"}"#)
                .profile()
                .unwrap_err()
                .error
                .code,
            "needsAuth"
        );
    }
    #[test]
    fn ambiguous_profile_write_is_rechecked_before_restoring_partial_success() {
        let mut w = worker();
        let end = clock::utc_ms() / 1000 + 600;
        let g = Guard {
            session_id: "s".into(),
            generation: 1,
            end_utc: end * 1000,
        };
        w.control.set_desired(Some(g.clone()));
        let (api, server) = sequence(
            vec![
                ("users.profile.get", json!({"ok":true,"profile":{}})),
                ("dnd.info", json!({"ok":true,"snooze_enabled":false})),
                ("dnd.setSnooze", json!({"ok":true,"snooze_endtime":end})),
                (
                    "users.profile.set",
                    json!({"ok":false,"error":"internal_error"}),
                ),
            ],
            None,
        );
        w.api = Some(api);
        assert!(w.protect(&g).unwrap_err().error.retryable);
        server.join().unwrap();
        let j =
            w.db.load_json::<Journal>("slack_guard_journal")
                .unwrap()
                .unwrap();
        assert!(j.profile_pending && j.snooze_pending);
        let (api, server) = sequence(
            vec![
                (
                    "dnd.info",
                    json!({"ok":true,"snooze_enabled":true,"snooze_endtime":end}),
                ),
                ("dnd.endSnooze", json!({"ok":true})),
                ("users.profile.get", json!({"ok":true,"profile":j.applied})),
                ("users.profile.set", json!({"ok":true})),
            ],
            None,
        );
        w.api = Some(api);
        w.restore(false).unwrap();
        server.join().unwrap();
        assert!(w.journal.is_none());
    }
    #[test]
    fn disconnect_keeps_credentials_while_manual_restoration_is_pending() {
        let mut w = worker();
        let mut j = journal();
        j.manual = true;
        j.snooze_pending = false;
        w.journal = Some(j);
        w.api = Some(Api::new("mock-only".into()).unwrap());
        assert_eq!(
            w.operation("slack_disconnect", None).unwrap_err().code,
            "restorePending"
        );
        assert!(w.api.is_some());
    }
    #[test]
    fn retries_are_bounded_and_authentication_failure_stops_automatic_retry() {
        let mut w = worker();
        w.journal = Some(journal());
        for _ in 0..8 {
            w.fail(AppError::new("network", "safe error", true).into());
        }
        assert!(!w.blocked);
        assert!(w.retry_at - clock::utc_ms() <= 60000);
        assert!(
            w.db.load_json::<Journal>("slack_guard_journal")
                .unwrap()
                .unwrap()
                .next_attempt
                > 0
        );
        w.fail(AppError::new("needsAuth", "safe error", false).into());
        assert!(w.blocked);
    }
}
