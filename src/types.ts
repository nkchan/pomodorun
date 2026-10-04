export type Phase = 'focus' | 'shortBreak' | 'longBreak';
export interface Settings {
  focusMinutes: number; shortBreakMinutes: number; longBreakMinutes: number;
  longBreakInterval: number; notifications: boolean; sound: boolean;
  shortcut: string; slackEnabled: boolean; theme: 'system' | 'light' | 'dark';
}
export interface Task {
  id: string; title: string; scheduledDate: string; estimatedPomodoros: number;
  completedPomodoros: number; completedAt: number | null; archivedAt: number | null;
  createdAt: number; updatedAt: number;
}
export interface Dump { id: string; text: string; capturedAt: number; convertedTaskId: string | null }
export interface SlackState { status: string; message: string; tokenSaved: boolean; manualRestoreAvailable: boolean }
export interface Snapshot {
  warning?: { code: string; message: string; retryable: boolean } | null;
  revision: number; settings: Settings; selectedTaskId: string | null; slack: SlackState;
  timer: { phase: Phase; status: 'idle' | 'running' | 'paused'; remainingMs: number; completedInCycle: number; session: { id: string; taskTitleSnapshot: string | null } | null };
}
export const today = () => { const d = new Date(); return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`; };
export const timeLabel = (ms: number) => { const s = Math.ceil(Math.max(0, ms) / 1000); return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`; };
