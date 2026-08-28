use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDecision {
    Continue,
    StopCurrent,
    StopAll,
}

#[derive(Debug, Default)]
struct ControlState {
    paused: bool,
    pause_after_current: bool,
    preview_enabled: bool,
    cpu_thread_limit: usize,
    stop: StopRequest,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum StopRequest {
    #[default]
    None,
    Current,
    All,
}

#[derive(Debug, Default)]
pub struct ConversionControl {
    state: Mutex<ControlState>,
    wake: Condvar,
}

impl ConversionControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        self.lock_state().paused = true;
    }

    pub fn pause_after_current(&self) {
        self.lock_state().pause_after_current = true;
    }

    pub fn complete_current(&self) {
        let mut state = self.lock_state();
        if state.stop == StopRequest::Current {
            state.stop = StopRequest::None;
        }
        if state.pause_after_current {
            state.pause_after_current = false;
            state.paused = true;
        }
    }

    pub fn resume(&self) {
        let mut state = self.lock_state();
        state.paused = false;
        self.wake.notify_all();
    }

    pub fn stop_current(&self) {
        let mut state = self.lock_state();
        if state.stop != StopRequest::All {
            state.stop = StopRequest::Current;
        }
        self.wake.notify_all();
    }

    pub fn stop_all(&self) {
        let mut state = self.lock_state();
        state.stop = StopRequest::All;
        state.paused = false;
        self.wake.notify_all();
    }

    pub fn set_preview_enabled(&self, enabled: bool) {
        self.lock_state().preview_enabled = enabled;
    }

    pub fn set_cpu_thread_limit(&self, threads: usize) {
        self.lock_state().cpu_thread_limit = threads;
    }

    #[must_use]
    pub fn cpu_thread_limit(&self) -> usize {
        self.lock_state().cpu_thread_limit
    }

    #[must_use]
    pub fn preview_enabled(&self) -> bool {
        self.lock_state().preview_enabled
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.lock_state().paused
    }

    #[must_use]
    pub fn checkpoint(&self) -> ControlDecision {
        let mut state = self.lock_state();
        while state.paused && state.stop == StopRequest::None {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        match state.stop {
            StopRequest::None => ControlDecision::Continue,
            StopRequest::Current => ControlDecision::StopCurrent,
            StopRequest::All => ControlDecision::StopAll,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ControlState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlDecision, ConversionControl};

    #[test]
    fn pause_after_current_activates_at_boundary() {
        let control = ConversionControl::new();
        control.pause_after_current();
        assert!(!control.is_paused());
        control.complete_current();
        assert!(control.is_paused());
        control.resume();
        assert_eq!(control.checkpoint(), ControlDecision::Continue);
    }

    #[test]
    fn stop_all_takes_priority() {
        let control = ConversionControl::new();
        control.stop_current();
        control.stop_all();
        assert_eq!(control.checkpoint(), ControlDecision::StopAll);
    }

    #[test]
    fn stop_current_is_distinct_from_stopping_the_queue() {
        let control = ConversionControl::new();
        control.stop_current();
        assert_eq!(control.checkpoint(), ControlDecision::StopCurrent);
        control.complete_current();
        assert_eq!(control.checkpoint(), ControlDecision::Continue);
    }

    #[test]
    fn preview_can_be_toggled_while_work_is_active() {
        let control = ConversionControl::new();
        assert!(!control.preview_enabled());
        control.set_preview_enabled(true);
        assert!(control.preview_enabled());
        control.set_preview_enabled(false);
        assert!(!control.preview_enabled());
    }

    #[test]
    fn cpu_thread_limit_can_change_while_work_is_active() {
        let control = ConversionControl::new();
        assert_eq!(control.cpu_thread_limit(), 0);
        control.set_cpu_thread_limit(6);
        assert_eq!(control.cpu_thread_limit(), 6);
        control.set_cpu_thread_limit(0);
        assert_eq!(control.cpu_thread_limit(), 0);
    }
}
