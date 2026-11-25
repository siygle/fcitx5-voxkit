//! Lazy load and idle unload for the ASR and translation models.
//!
//! A model is loaded when it is first needed (preloading starts as soon as
//! the push-to-talk key goes down) and unloaded after it has been idle for
//! the configured time. While a request holds a [`Busy`] guard the model is
//! never unloaded, however long the request takes.
//!
//! `VOXKIT_IDLE_UNLOAD` sets the idle time in seconds for both models
//! (default 600; `0` or `never` keeps them loaded). `VOXKIT_MT_IDLE_UNLOAD`
//! overrides it for the translation model only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_IDLE: Duration = Duration::from_secs(600);

/// Parse an idle setting: seconds, or `never`/`off`/`0` for no unloading.
pub fn parse_idle(value: &str) -> Option<Option<Duration>> {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "never" | "off" | "0" => Some(None),
        _ => value
            .parse::<u64>()
            .ok()
            .map(|secs| Some(Duration::from_secs(secs))),
    }
}

fn idle_from_env(name: &str, fallback: Option<Duration>) -> Option<Duration> {
    match std::env::var(name) {
        Ok(value) => match parse_idle(&value) {
            Some(idle) => idle,
            None => {
                eprintln!("voxkit: ignoring {name}={value} (seconds or 'never')");
                fallback
            }
        },
        Err(_) => fallback,
    }
}

/// Idle time for the ASR model.
pub fn asr_idle() -> Option<Duration> {
    idle_from_env("VOXKIT_IDLE_UNLOAD", Some(DEFAULT_IDLE))
}

/// Idle time for the translation model.
pub fn mt_idle() -> Option<Duration> {
    idle_from_env("VOXKIT_MT_IDLE_UNLOAD", asr_idle())
}

/// Use tracking for one model.
pub struct Lifecycle {
    name: &'static str,
    idle: Option<Duration>,
    inflight: AtomicUsize,
    last_used: Mutex<Instant>,
}

/// Held for the whole of a request (key down → text delivered or cancelled).
pub struct Busy {
    owner: Arc<Lifecycle>,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.owner.touch();
        self.owner.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lifecycle {
    pub fn new(name: &'static str, idle: Option<Duration>) -> Arc<Self> {
        Arc::new(Lifecycle {
            name,
            idle,
            inflight: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn idle(&self) -> Option<Duration> {
        self.idle
    }

    pub fn busy(self: &Arc<Self>) -> Busy {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Busy {
            owner: Arc::clone(self),
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    pub fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// True when nothing is in flight and the model has been idle long enough.
    pub fn expired(&self, now: Instant) -> bool {
        let Some(idle) = self.idle else {
            return false;
        };
        if self.inflight() > 0 {
            return false;
        }
        let last = *self.last_used.lock().unwrap_or_else(|e| e.into_inner());
        now.saturating_duration_since(last) >= idle
    }
}

/// Something the reaper can unload.
pub trait Unload: Send {
    /// Whether a model is loaded now.
    fn loaded(&self) -> bool;
    fn unload(&mut self);
}

/// One reaper pass: unload `slot` if its lifecycle expired. The in-flight
/// check is repeated while holding the slot's lock, and the lock is only
/// tried (a request using the model holds it), so a running request is never
/// unloaded under it. Returns true when something was unloaded.
pub fn reap<T: Unload>(lifecycle: &Lifecycle, slot: &Mutex<T>, now: Instant) -> bool {
    if !lifecycle.expired(now) {
        return false;
    }
    let Ok(mut guard) = slot.try_lock() else {
        return false;
    };
    if !guard.loaded() || !lifecycle.expired(now) {
        return false;
    }
    guard.unload();
    eprintln!(
        "voxkit: unloaded {} model after {} s idle",
        lifecycle.name(),
        lifecycle.idle().map(|d| d.as_secs()).unwrap_or(0)
    );
    true
}

/// Background thread that runs [`reap`] for each model every few seconds.
pub fn spawn_reaper<A, B>(
    asr: (Arc<Lifecycle>, Arc<Mutex<A>>),
    mt: (Arc<Lifecycle>, Arc<Mutex<B>>),
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    A: Unload + 'static,
    B: Unload + 'static,
{
    std::thread::spawn(move || {
        let shortest = [asr.0.idle(), mt.0.idle()]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(Duration::from_secs(3600));
        let period = (shortest / 4).clamp(Duration::from_millis(50), Duration::from_secs(15));
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(period);
            let now = Instant::now();
            reap(&asr.0, &asr.1, now);
            reap(&mt.0, &mt.1, now);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        loaded: bool,
        unloads: usize,
    }

    impl Unload for Fake {
        fn loaded(&self) -> bool {
            self.loaded
        }
        fn unload(&mut self) {
            self.loaded = false;
            self.unloads += 1;
        }
    }

    #[test]
    fn parse_idle_values() {
        assert_eq!(parse_idle("never"), Some(None));
        assert_eq!(parse_idle("0"), Some(None));
        assert_eq!(parse_idle(" 300 "), Some(Some(Duration::from_secs(300))));
        assert_eq!(parse_idle("soon"), None);
    }

    #[test]
    fn unloads_after_idle_but_never_during_a_request() {
        let lc = Lifecycle::new("test", Some(Duration::from_millis(30)));
        let slot = Mutex::new(Fake {
            loaded: true,
            unloads: 0,
        });

        // A request starts and runs far longer than the idle time.
        let busy = lc.busy();
        std::thread::sleep(Duration::from_millis(80));
        assert!(!lc.expired(Instant::now()));
        assert!(!reap(&lc, &slot, Instant::now()));
        assert!(slot.lock().unwrap().loaded, "unloaded during a request");

        // Request done: the idle clock restarts at its end.
        drop(busy);
        assert!(!reap(&lc, &slot, Instant::now()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(reap(&lc, &slot, Instant::now()));
        let fake = slot.lock().unwrap();
        assert!(!fake.loaded);
        assert_eq!(fake.unloads, 1);
    }

    #[test]
    fn a_held_lock_is_not_unloaded() {
        let lc = Lifecycle::new("test", Some(Duration::from_millis(1)));
        let slot = Mutex::new(Fake {
            loaded: true,
            unloads: 0,
        });
        std::thread::sleep(Duration::from_millis(5));
        let guard = slot.lock().unwrap(); // a load/transcribe in progress
        assert!(!reap(&lc, &slot, Instant::now()));
        drop(guard);
        assert!(reap(&lc, &slot, Instant::now()));
    }

    #[test]
    fn never_means_never() {
        let lc = Lifecycle::new("test", None);
        let slot = Mutex::new(Fake {
            loaded: true,
            unloads: 0,
        });
        assert!(!reap(
            &lc,
            &slot,
            Instant::now() + Duration::from_secs(86_400)
        ));
        assert!(slot.lock().unwrap().loaded);
    }

    #[test]
    fn reaper_thread_unloads_idle_model() {
        let asr = Lifecycle::new("asr", Some(Duration::from_millis(40)));
        let mt = Lifecycle::new("mt", None);
        let asr_slot = Arc::new(Mutex::new(Fake {
            loaded: true,
            unloads: 0,
        }));
        let mt_slot = Arc::new(Mutex::new(Fake {
            loaded: true,
            unloads: 0,
        }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = spawn_reaper(
            (Arc::clone(&asr), Arc::clone(&asr_slot)),
            (Arc::clone(&mt), Arc::clone(&mt_slot)),
            Arc::clone(&stop),
        );
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert!(!asr_slot.lock().unwrap().loaded);
        assert!(
            mt_slot.lock().unwrap().loaded,
            "idle=never must stay loaded"
        );
    }
}
