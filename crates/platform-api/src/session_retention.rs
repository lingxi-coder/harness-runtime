//! Process-local authority retention. Caches own cores; external views own
//! pins. Neither a pin nor a retirement token is a serializable capability.

use std::sync::{Arc, Mutex, Weak};

/// Fail-closed errors when pinning or retiring process-local session authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRetentionError {
    /// The gate is retiring/retired, poisoned, or not in the required phase.
    Unavailable,
    /// A pin count or monotonic retirement epoch cannot be incremented safely.
    Exhausted,
    /// The supplied retirement token no longer matches the gate's current epoch.
    WrongEpoch,
}

impl std::fmt::Display for SessionRetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "session authority is retiring or retired",
            Self::Exhausted => "session retention counter exhausted",
            Self::WrongEpoch => "session retirement epoch does not match",
        })
    }
}
impl std::error::Error for SessionRetentionError {}

#[derive(Default)]
struct State {
    pins: usize,
    epoch: u64,
    phase: Phase,
    on_idle: Option<Weak<dyn Fn() + Send + Sync>>,
}

#[derive(Default, Clone, Copy)]
enum Phase {
    #[default]
    Live,
    Retiring {
        closing: bool,
    },
    Retired,
}

/// Shared identity and pin count for one cached session authority.
/// Cloning the gate does not pin it; external views must acquire retention pins.
#[derive(Clone, Default)]
pub struct SessionRetentionGate(Arc<Mutex<State>>);

impl SessionRetentionGate {
    /// The owner coalesces wakeups. Keep only a weak callback so the retention
    /// authority never owns the cache manager, and never invoke it under this
    /// gate's lock. Installing a callback does not manufacture an idle event.
    pub fn set_idle_callback(&self, callback: Weak<dyn Fn() + Send + Sync>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .on_idle = Some(callback);
    }

    /// Acquire an external-view pin while the authority is Live.
    /// Its final clone's drop releases the pin; an active retirement rejects acquisition.
    pub fn try_pin(&self) -> Result<SessionRetentionPin, SessionRetentionError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| SessionRetentionError::Unavailable)?;
        if !matches!(state.phase, Phase::Live) {
            return Err(SessionRetentionError::Unavailable);
        }
        state.pins = state
            .pins
            .checked_add(1)
            .ok_or(SessionRetentionError::Exhausted)?;
        Ok(SessionRetentionPin(Arc::new(PinOwner(self.clone()))))
    }

    /// None means live external views still pin the authority. Epochs are
    /// monotonic for this gate, including attempts rolled back before closing.
    pub fn try_begin_retirement(&self) -> Result<Option<SessionRetirement>, SessionRetentionError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| SessionRetentionError::Unavailable)?;
        if !matches!(state.phase, Phase::Live) {
            return Err(SessionRetentionError::Unavailable);
        }
        if state.pins != 0 {
            return Ok(None);
        }
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or(SessionRetentionError::Exhausted)?;
        state.phase = Phase::Retiring { closing: false };
        Ok(Some(SessionRetirement {
            gate: self.clone(),
            epoch: state.epoch,
        }))
    }

    /// Whether the gate currently permits pin acquisition; not a reservation
    /// and not proof that the authority is idle or eligible for retirement.
    pub fn is_live(&self) -> bool {
        self.0
            .lock()
            .is_ok_and(|state| matches!(state.phase, Phase::Live))
    }

    /// Compare the shared gate identity, rather than session labels or pin counts.
    pub fn shares_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

struct PinOwner(SessionRetentionGate);
impl Drop for PinOwner {
    fn drop(&mut self) {
        let mut state = self
            .0
             .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pins = state
            .pins
            .checked_sub(1)
            .expect("retention pin counted exactly once");
        let callback = (state.pins == 0 && matches!(state.phase, Phase::Live))
            .then(|| state.on_idle.as_ref().and_then(Weak::upgrade))
            .flatten();
        drop(state);
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// Clones keep the same external view family alive. Caches must not retain
/// this view: an external Weak must point at a view, not a cached core.
#[derive(Clone)]
pub struct SessionRetentionPin(Arc<PinOwner>);

impl SessionRetentionPin {
    /// Whether this pin was minted by the specified gate.
    pub fn belongs_to(&self, gate: &SessionRetentionGate) -> bool {
        self.0 .0.shares_authority(gate)
    }
}

/// Exclusive retirement epoch that temporarily blocks new external pins.
/// Dropping before `begin_close` restores Live; after close begins it never
/// resurrects authority, even when the owner's later cleanup fails.
pub struct SessionRetirement {
    gate: SessionRetentionGate,
    epoch: u64,
}

impl SessionRetirement {
    /// Monotonic epoch assigned by this gate when retirement was admitted.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether this token shares the specified gate identity; this alone does
    /// not validate that its epoch or close phase is still current.
    pub fn belongs_to(&self, gate: &SessionRetentionGate) -> bool {
        self.gate.shares_authority(gate)
    }

    /// Call immediately before starting irreversible coordinator close. After
    /// this point dropping the token must not resurrect the authority.
    pub fn begin_close(&mut self) -> Result<(), SessionRetentionError> {
        let mut state = self
            .gate
            .0
            .lock()
            .map_err(|_| SessionRetentionError::Unavailable)?;
        if state.epoch != self.epoch {
            return Err(SessionRetentionError::WrongEpoch);
        }
        match &mut state.phase {
            Phase::Retiring { closing } => {
                *closing = true;
                Ok(())
            }
            _ => Err(SessionRetentionError::Unavailable),
        }
    }

    /// Complete only after the owner has drained and closed all durable work.
    pub fn finish(self) -> Result<(), SessionRetentionError> {
        let mut state = self
            .gate
            .0
            .lock()
            .map_err(|_| SessionRetentionError::Unavailable)?;
        if state.epoch != self.epoch {
            return Err(SessionRetentionError::WrongEpoch);
        }
        if !matches!(state.phase, Phase::Retiring { closing: true }) {
            return Err(SessionRetentionError::Unavailable);
        }
        state.phase = Phase::Retired;
        Ok(())
    }
}

impl Drop for SessionRetirement {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.epoch == self.epoch && matches!(state.phase, Phase::Retiring { closing: false }) {
            state.phase = Phase::Live;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_pin_idle_callback_is_weak_and_runs_outside_gate_lock() {
        let gate = SessionRetentionGate::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let gate = gate.clone();
            let calls = calls.clone();
            move || {
                assert!(gate.is_live());
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        gate.set_idle_callback(Arc::downgrade(&callback));
        let first = gate.try_pin().unwrap();
        let first_clone = first.clone();
        let second = gate.try_pin().unwrap();
        drop(first);
        drop(first_clone);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A speculative retirement rollback is not another wakeup.
        drop(gate.try_begin_retirement().unwrap().unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let final_pin = gate.try_pin().unwrap();
        drop(callback);
        drop(final_pin);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn live_pin_and_clones_block_retirement_until_last_drop() {
        let gate = SessionRetentionGate::default();
        let pin = gate.try_pin().unwrap();
        let clone = pin.clone();
        assert!(pin.belongs_to(&gate));
        assert!(!pin.belongs_to(&SessionRetentionGate::default()));
        drop(pin);
        assert!(gate.try_begin_retirement().unwrap().is_none());
        drop(clone);
        assert!(gate.try_begin_retirement().unwrap().is_some());
    }

    #[test]
    fn rollback_advances_epoch_but_close_never_resurrects() {
        let gate = SessionRetentionGate::default();
        let first = gate.try_begin_retirement().unwrap().unwrap();
        let epoch = first.epoch();
        assert!(gate.try_pin().is_err());
        drop(first);
        assert!(gate.is_live());
        let mut second = gate.try_begin_retirement().unwrap().unwrap();
        assert!(second.epoch() > epoch);
        second.begin_close().unwrap();
        drop(second);
        assert!(!gate.is_live());
        assert!(gate.try_pin().is_err());
    }

    #[test]
    fn finished_authority_stays_retired() {
        let gate = SessionRetentionGate::default();
        let mut token = gate.try_begin_retirement().unwrap().unwrap();
        assert!(token.belongs_to(&gate));
        token.begin_close().unwrap();
        token.finish().unwrap();
        assert!(gate.try_pin().is_err());
        assert!(gate.try_begin_retirement().is_err());
    }

    #[test]
    fn external_weak_view_cannot_revive_from_cached_core() {
        let cache = Arc::new(());
        let gate = SessionRetentionGate::default();
        let view = Arc::new((cache.clone(), gate.try_pin().unwrap()));
        let weak = Arc::downgrade(&view);
        drop(view);
        let token = gate.try_begin_retirement().unwrap().unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(Arc::strong_count(&cache), 1);
        drop(token);
    }

    #[test]
    fn pin_and_retirement_race_has_only_one_winner() {
        for _ in 0..32 {
            let gate = SessionRetentionGate::default();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = gate.clone();
            let start = barrier.clone();
            let pin = std::thread::spawn(move || {
                start.wait();
                other.try_pin()
            });
            barrier.wait();
            let retirement = gate.try_begin_retirement().unwrap();
            let pin = pin.join().unwrap();
            assert_ne!(pin.is_ok(), retirement.is_some());
        }
    }

    #[test]
    fn counter_exhaustion_does_not_mutate_authority() {
        let gate = SessionRetentionGate::default();
        gate.0.lock().unwrap().epoch = u64::MAX;
        assert!(matches!(
            gate.try_begin_retirement(),
            Err(SessionRetentionError::Exhausted)
        ));
        assert!(gate.is_live());
        gate.0.lock().unwrap().pins = usize::MAX;
        assert!(matches!(
            gate.try_pin(),
            Err(SessionRetentionError::Exhausted)
        ));
        assert_eq!(gate.0.lock().unwrap().pins, usize::MAX);
    }
}
