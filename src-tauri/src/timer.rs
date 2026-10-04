use crate::model::*;

impl Timer {
    pub fn remaining(&self, now: u64) -> u64 {
        self.deadline_mono
            .map(|d| d.saturating_sub(now))
            .unwrap_or(self.remaining_ms)
    }
    fn begin(
        &mut self,
        phase: Phase,
        task: Option<&Task>,
        settings: &Settings,
        mono: u64,
        utc: i64,
    ) {
        self.phase = phase;
        self.status = Status::Running;
        self.remaining_ms = settings.duration(phase);
        self.deadline_mono = Some(mono.saturating_add(self.remaining_ms));
        self.deadline_utc = Some(utc + self.remaining_ms as i64);
        self.generation += 1;
        self.session = Some(Session {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task.map(|t| t.id.clone()),
            task_title_snapshot: task.map(|t| t.title.clone()),
            phase,
            planned_seconds: self.remaining_ms / 1000,
            elapsed_seconds: 0,
            started_at: utc,
            ended_at: None,
            outcome: "active".into(),
        });
    }
    pub fn start(
        &mut self,
        task: Option<&Task>,
        settings: &Settings,
        mono: u64,
        utc: i64,
    ) -> Result<()> {
        if self.status != Status::Idle {
            return Err(AppError::invalid("すでにセッションが進行中です。"));
        }
        self.begin(Phase::Focus, task, settings, mono, utc);
        Ok(())
    }
    pub fn pause(&mut self, mono: u64) -> Result<()> {
        if self.status != Status::Running {
            return Err(AppError::invalid("実行中のタイマーがありません。"));
        }
        self.remaining_ms = self.remaining(mono);
        self.deadline_mono = None;
        self.deadline_utc = None;
        self.status = Status::Paused;
        self.generation += 1;
        self.update_elapsed();
        Ok(())
    }
    pub fn resume(&mut self, mono: u64, utc: i64) -> Result<()> {
        if self.status != Status::Paused {
            return Err(AppError::invalid("一時停止中のタイマーがありません。"));
        }
        self.deadline_mono = Some(mono + self.remaining_ms);
        self.deadline_utc = Some(utc + self.remaining_ms as i64);
        self.status = Status::Running;
        self.generation += 1;
        Ok(())
    }
    fn update_elapsed(&mut self) {
        if let Some(s) = &mut self.session {
            s.elapsed_seconds = s
                .planned_seconds
                .saturating_sub(self.remaining_ms.div_ceil(1000));
        }
    }
    fn finish(&mut self, outcome: &str, utc: i64) -> Option<Session> {
        self.update_elapsed();
        self.session.take().map(|mut s| {
            s.outcome = outcome.into();
            s.ended_at = Some(utc);
            s
        })
    }
    fn idle(&mut self, settings: &Settings) {
        self.phase = Phase::Focus;
        self.status = Status::Idle;
        self.remaining_ms = settings.duration(Phase::Focus);
        self.deadline_mono = None;
        self.deadline_utc = None;
        self.generation += 1;
    }
    pub fn stop(&mut self, settings: &Settings, mono: u64, utc: i64) -> Option<Session> {
        self.remaining_ms = self.remaining(mono);
        let s = self.finish("interrupted", utc);
        self.idle(settings);
        s
    }
    pub fn recover(&mut self, settings: &Settings, utc: i64) -> Option<Session> {
        let s = self.finish("interrupted", utc);
        self.idle(settings);
        s
    }
    /// Catch up from the original monotonic deadline, including an entire break spent asleep.
    pub fn advance(&mut self, settings: &Settings, mono: u64, utc: i64) -> Vec<Session> {
        let mut done = Vec::new();
        while self.status == Status::Running && self.remaining(mono) == 0 {
            let deadline = self.deadline_mono.unwrap_or(mono);
            let end_utc = utc - mono.saturating_sub(deadline) as i64;
            self.remaining_ms = 0;
            if let Some(s) = self.finish("completed", end_utc) {
                done.push(s);
            }
            if self.phase == Phase::Focus {
                self.completed_in_cycle += 1;
                let next = if self.completed_in_cycle >= settings.long_break_interval {
                    self.completed_in_cycle = 0;
                    Phase::LongBreak
                } else {
                    Phase::ShortBreak
                };
                self.begin(next, None, settings, deadline, end_utc);
            } else {
                self.idle(settings);
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_deadline_operations_never_double_complete() {
        let mut t = Timer::default();
        let s = settings();
        t.start(None, &s, 0, 0).unwrap();
        assert_eq!(t.advance(&s, 60000, 60000).len(), 1);
        t.pause(60000).unwrap();
        assert!(t.pause(60000).is_err());
        t.resume(60000, 60000).unwrap();
        assert!(t.resume(60000, 60000).is_err());
        assert!(t.advance(&s, 60000, 60000).is_empty());
        assert_eq!(t.completed_in_cycle, 1);
        assert_eq!(t.stop(&s, 60000, 60000).unwrap().outcome, "interrupted");
        assert!(t.stop(&s, 60000, 60000).is_none());
        assert_eq!(t.completed_in_cycle, 1);
    }
    fn settings() -> Settings {
        Settings {
            focus_minutes: 1,
            short_break_minutes: 1,
            long_break_minutes: 2,
            ..Settings::default()
        }
    }
    #[test]
    fn fourth_focus_gets_long_break() {
        let mut t = Timer::default();
        let s = settings();
        for i in 1..=4 {
            let n = i * 200_000;
            t.start(None, &s, n, 0).unwrap();
            assert_eq!(t.advance(&s, n + 60_000, 60_000).len(), 1);
            assert_eq!(
                t.phase,
                if i == 4 {
                    Phase::LongBreak
                } else {
                    Phase::ShortBreak
                }
            );
            t.stop(&s, n + 60_000, 60_000);
        }
        assert_eq!(t.completed_in_cycle, 0);
    }
    #[test]
    fn sleep_catches_up_without_auto_focus_or_double_count() {
        let mut t = Timer::default();
        let s = settings();
        t.start(None, &s, 100, 0).unwrap();
        let done = t.advance(&s, 180_100, 180_000);
        assert_eq!(done.len(), 2);
        assert_eq!(done[0].ended_at, Some(60_000));
        assert_eq!(done[1].ended_at, Some(120_000));
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.completed_in_cycle, 1);
        assert!(t.advance(&s, 200_000, 200_000).is_empty());
    }
    #[test]
    fn pause_freezes_and_wall_clock_jump_does_not_complete() {
        let mut t = Timer::default();
        let s = settings();
        t.start(None, &s, 0, 0).unwrap();
        t.pause(10_000).unwrap();
        assert_eq!(t.remaining(999_999), 50_000);
        t.resume(1_000_000, -9_000_000).unwrap();
        assert!(t.advance(&s, 1_049_999, 9_000_000).is_empty());
        assert_eq!(t.advance(&s, 1_050_000, 9_000_001).len(), 1);
    }
    #[test]
    fn settings_only_apply_to_next_session() {
        let mut t = Timer::default();
        let mut s = settings();
        t.start(None, &s, 0, 0).unwrap();
        s.focus_minutes = 180;
        s.short_break_minutes = 3;
        assert_eq!(t.remaining(0), 60_000);
        t.advance(&s, 60_000, 60_000);
        assert_eq!(t.remaining(60_000), 180_000);
    }
    #[test]
    fn restart_and_stop_never_count() {
        let mut t = Timer::default();
        let s = settings();
        t.start(None, &s, 0, 0).unwrap();
        t.pause(10_000).unwrap();
        let done = t.recover(&s, 9_000_000).unwrap();
        assert_eq!(done.outcome, "interrupted");
        assert_eq!(done.elapsed_seconds, 10);
        assert_eq!(t.completed_in_cycle, 0);
        assert!(t.start(None, &s, 0, 0).is_ok());
        assert!(t.start(None, &s, 0, 0).is_err());
    }
}
