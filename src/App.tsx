import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { today, timeLabel, type Dump, type Settings, type SlackState, type Snapshot, type Task } from './types';

type Tab = 'timer' | 'dump' | 'settings';
const ipc = <T,>(command: string, args: unknown = {}) => invoke<T>(command, { args });
export default function App() {
  const [state, setState] = useState<Snapshot | null>(null);
  const current = useRef<Snapshot | null>(null);
  const [tasks, setTasks] = useState<Task[]>([]);
  const [dumps, setDumps] = useState<Dump[]>([]);
  const [tab, setTab] = useState<Tab>('timer');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState('');
  const [date, setDate] = useState(today());
  const dateRef = useRef(date); dateRef.current = date;
  const [title, setTitle] = useState('');
  const [estimate, setEstimate] = useState(1);
  const [editing, setEditing] = useState<Task | null>(null);
  const [memo, setMemo] = useState('');
  const memoRef = useRef<HTMLTextAreaElement>(null);
  const [draft, setDraft] = useState<Settings | null>(null);
  const [token, setToken] = useState('');
  const [notice, setNotice] = useState('');
  const listVersion = useRef(0);
  const accept = useCallback((s: Snapshot) => {
    if (s.revision < (current.current?.revision ?? -1)) return;
    current.current = s; setState(s); setDraft(d => d ?? s.settings);
  }, []);
  const report = useCallback((e: unknown) => setError(typeof e === 'object' && e && 'message' in e ? String(e.message) : '操作に失敗しました。再試行してください。'), []);
  const lists = useCallback(async () => {
    const v = ++listVersion.current;
    const [t, d] = await Promise.all([ipc<Task[]>('tasks_list', { date: dateRef.current }), ipc<Dump[]>('brain_dump_list')]);
    if (v === listVersion.current) { setTasks(t); setDumps(d); }
  }, []);
  const sync = useCallback(async () => { accept(await ipc<Snapshot>('get_app_state')); await lists(); }, [accept, lists]);
  const act = async (name: string, args: unknown = {}, after?: () => void) => {
    setBusy(true); setError(''); setNotice('');
    try { const result = await ipc<unknown>(name, args); if (result && typeof result === 'object' && 'message' in result) setNotice(String(result.message)); after?.(); await sync(); }
    catch (e) { report(e); } finally { setBusy(false); }
  };
  useEffect(() => {
    let disposed = false; const cleanups: UnlistenFn[] = [];
    const subscribe = async <T,>(name: string, cb: (payload: T) => void) => {
      const off = await listen<T>(name, e => { if (!disposed) cb(e.payload); });
      if (disposed) off(); else cleanups.push(off);
    };
    void (async () => {
      await Promise.all([
        subscribe<unknown>('app:error', report),
        subscribe<{ snapshot: Snapshot }>('app:changed', p => { accept(p.snapshot); void lists().catch(report); }),
        subscribe<{ revision: number; sessionId: string; remainingMs: number }>('timer:tick', p => {
          const s = current.current;
          if (!s || p.revision > s.revision) { void sync().catch(report); return; }
          if (p.revision === s.revision && s.timer.session?.id === p.sessionId) accept({ ...s, timer: { ...s.timer, remainingMs: p.remainingMs } });
        }),
        subscribe<SlackState>('slack:changed', p => { const s = current.current; if (s) accept({ ...s, slack: p }); }),
        subscribe<Tab>('ui:show', p => { setTab(p); if (p === 'dump') requestAnimationFrame(() => memoRef.current?.focus()); if (p === 'settings') setDraft(current.current?.settings ?? null); setDate(today()); void sync().catch(report); }),
      ]);
      if (!disposed) await sync();
    })().catch(report);
    const visible = () => { if (document.visibilityState === 'visible') { setDate(today()); void sync().catch(report); } };
    const focus = () => { setDate(today()); void sync().catch(report); };
    const escape = (e: KeyboardEvent) => { if (e.key === 'Escape') void getCurrentWindow().hide().catch(report); };
    document.addEventListener('visibilitychange', visible); window.addEventListener('focus', focus); window.addEventListener('keydown', escape);
    return () => { disposed = true; cleanups.forEach(off => off()); document.removeEventListener('visibilitychange', visible); window.removeEventListener('focus', focus); window.removeEventListener('keydown', escape); };
  }, [accept, lists, report, sync]);
  useEffect(() => { void lists().catch(report); }, [date, lists, report]);
  useEffect(() => { if (tab === 'dump') memoRef.current?.focus(); }, [tab]);
  useEffect(() => { document.documentElement.dataset.theme = state?.settings.theme ?? 'system'; }, [state?.settings.theme]);
  const switchTab = (t: Tab) => { setTab(t); if (t === 'settings') setDraft(state?.settings ?? null); };
  const phase = state?.timer.phase === 'focus' ? '集中' : state?.timer.phase === 'longBreak' ? '長い休憩' : '短い休憩';
  return <main>
    <header><strong><span className="tomato">●</span> Pomodorun</strong><button aria-label="閉じる" onClick={() => void getCurrentWindow().hide().catch(report)}>×</button></header>
    <nav aria-label="画面切り替え">{([['timer', '集中'], ['dump', 'Brain Dump'], ['settings', '設定']] as const).map(([t, label]) => <button key={t} aria-pressed={tab === t} onClick={() => switchTab(t)}>{label}</button>)}</nav>
    {error && <div className="error" role="alert">{error}<button onClick={() => void sync().then(() => setError('')).catch(report)}>再取得</button></div>}
    {notice && <p role="status" className="notice">{notice}</p>}
    {state?.warning && <p role="alert" className="error">{state.warning.message}</p>}
    {!state ? <p role="status">読み込み中…</p> : <>
      {tab === 'timer' && <>
        <section className="timer"><p>{phase}{state.timer.status === 'paused' ? ' · 一時停止' : ''}</p><div className="digits" aria-label="残り時間">{timeLabel(state.timer.remainingMs)}</div><p className="muted">{state.timer.session?.taskTitleSnapshot ?? 'タスク未選択'} · {state.timer.completedInCycle}/{state.settings.longBreakInterval} 回</p>
          <div className="actions">
            {state.timer.status === 'idle' ? <button className="primary" disabled={busy} onClick={() => void act('timer_start', { taskId: selected || null })}>集中を開始</button> : <>
              <button className="primary" disabled={busy} onClick={() => void act(state.timer.status === 'running' ? 'timer_pause' : 'timer_resume')}>{state.timer.status === 'running' ? '一時停止' : '再開'}</button>
              <button disabled={busy} onClick={() => void act('timer_stop')}>停止</button>
              {state.timer.phase !== 'focus' && <button disabled={busy} onClick={() => void act('timer_skip_break')}>休憩スキップ</button>}
            </>}
          </div>
        </section>
        <p className={`slack ${state.slack.status === 'protecting' ? 'protected' : ''}`}>{state.slack.message}</p>
        <section><div className="section-heading"><h2>タスク</h2><input aria-label="表示日" type="date" value={date} onChange={e => setDate(e.target.value)} /></div>
          <label>次の集中タスク<select disabled={state.timer.status !== 'idle' || busy} value={selected} onChange={e => setSelected(e.target.value)}><option value="">選択しない</option>{tasks.filter(t => !t.completedAt).map(t => <option key={t.id} value={t.id}>{t.title}</option>)}</select></label>
          <form onSubmit={e => { e.preventDefault(); void act(editing ? 'task_update' : 'task_create', editing ? { task: { ...editing, title, estimatedPomodoros: estimate, scheduledDate: date } } : { title, date, estimate }, () => { setTitle(''); setEstimate(1); setEditing(null); }); }}>
            <label className="sr-only" htmlFor="title">タスク名</label><input id="title" value={title} onChange={e => setTitle(e.target.value)} placeholder="取り組むこと" maxLength={200} required />
            <div className="row"><label>見積もり<input type="number" min={1} max={99} value={estimate} onChange={e => setEstimate(Number(e.target.value))} required /></label><button disabled={busy}>{editing ? '更新' : '追加'}</button>{editing && <button type="button" onClick={() => { setEditing(null); setTitle(''); }}>取消</button>}</div>
          </form>
          <ul>{tasks.map(t => <li key={t.id}><div className="task"><input aria-label={`${t.title}を${t.completedAt ? '未完了に戻す' : '完了'}`} type="checkbox" checked={!!t.completedAt} disabled={busy} onChange={() => void act('task_update', { task: { ...t, completedAt: t.completedAt ? null : Date.now() } })} /><div><span className={t.completedAt ? 'done' : ''}>{t.title}</span><small>{t.scheduledDate < date && !t.completedAt ? `持ち越し · ${t.scheduledDate} · ` : ''}{t.completedPomodoros}/{t.estimatedPomodoros} 回</small></div></div><div className="item-actions"><button disabled={busy} onClick={() => { setEditing(t); setTitle(t.title); setEstimate(t.estimatedPomodoros); }}>編集</button><button disabled={busy} onClick={() => { if (selected === t.id) setSelected(''); void act('task_archive', { id: t.id }); }}>削除</button></div></li>)}</ul>
          {!tasks.length && <p className="muted">タスクを追加して、ひとつずつ。</p>}
        </section>
      </>}
      {tab === 'dump' && <section><h2>Brain Dump</h2><p className="muted">気になることを置いて、集中に戻ろう。</p><form onSubmit={e => { e.preventDefault(); void act('brain_dump_add', { text: memo }, () => { setMemo(''); memoRef.current?.focus(); }); }}><label htmlFor="memo">メモ</label><textarea id="memo" ref={memoRef} value={memo} onChange={e => setMemo(e.target.value)} maxLength={4000} required rows={5} placeholder="あとで考えたいこと…" /><button className="primary" disabled={busy || !memo.trim()}>保存</button></form><ul>{dumps.map(d => <li key={d.id}><p className="memo-text">{d.text}</p><small>{new Date(d.capturedAt).toLocaleString('ja-JP')}</small><div className="item-actions"><button disabled={busy} onClick={() => void act('brain_dump_convert', { id: d.id, date: today() })}>今日のタスクにする</button><button disabled={busy} onClick={() => void act('brain_dump_archive', { id: d.id })}>整理</button></div></li>)}</ul></section>}
      {tab === 'settings' && draft && <section><h2>設定</h2><form onSubmit={e => { e.preventDefault(); void act('settings_update', { settings: draft }); }}>
        {([['focusMinutes', '集中（分）'], ['shortBreakMinutes', '短休憩（分）'], ['longBreakMinutes', '長休憩（分）'], ['longBreakInterval', '長休憩までの集中回数']] as const).map(([key, label]) => <label className="setting-row" key={key}>{label}<input type="number" min={1} max={key === 'longBreakInterval' ? 12 : 180} required value={draft[key]} onChange={e => setDraft({ ...draft, [key]: Number(e.target.value) })} /></label>)}
        {([['notifications', '完了通知'], ['sound', '完了サウンド'], ['slackEnabled', 'Slack Focus Guard']] as const).map(([key, label]) => <label className="setting-row" key={key}>{label}<input type="checkbox" checked={draft[key]} onChange={e => setDraft({ ...draft, [key]: e.target.checked })} /></label>)}
        <label>Brain Dumpショートカット<input value={draft.shortcut} onChange={e => setDraft({ ...draft, shortcut: e.target.value })} required /></label><label>テーマ<select value={draft.theme} onChange={e => setDraft({ ...draft, theme: e.target.value as Settings['theme'] })}><option value="system">OSに合わせる</option><option value="light">ライト</option><option value="dark">ダーク</option></select></label><p className="muted">時間変更は次のセッションから適用されます。</p><button className="primary" disabled={busy}>設定を保存</button>
      </form><hr /><h2>Slack接続</h2><p className="slack">{state.slack.message}</p><p className="muted">{state.slack.tokenSaved ? 'トークン保存済み（Keychain）' : 'トークン未保存'} · 1ワークスペース対応</p><form onSubmit={e => { e.preventDefault(); void act('slack_save_token', { token }, () => setToken('')); }}><label htmlFor="token">ユーザートークン（xoxp-）</label><input id="token" type="password" autoComplete="off" value={token} onChange={e => setToken(e.target.value)} placeholder="xoxp-…" required /><button disabled={busy || !token}>Keychainに保存</button></form><div className="actions"><button disabled={busy || !state.slack.tokenSaved} onClick={() => void act('slack_test_connection')}>接続テスト（読取のみ）</button><button disabled={busy || !state.slack.tokenSaved} onClick={() => void act('slack_disconnect')}>切断</button></div><button disabled={busy} onClick={() => { if (!state.slack.manualRestoreAvailable || window.confirm('現在のSlackステータスを元の内容で上書きします。復元しますか？')) void act('slack_retry_restore'); }}>{state.slack.manualRestoreAvailable ? '元ステータスを明示復元' : '復元を再試行'}</button><p className="muted">必要なUser Token Scopes:<br />users.profile:read / users.profile:write<br />dnd:read / dnd:write<br />Slackの手動変更は保持します。比較更新は原子的ではなく、競合を完全には防げません。</p></section>}
    </>}
    <footer>ローカル保存 · 自分のペースで</footer>
  </main>;
}
