//! Bounded, content-free phase measurements for the run-cap runtime regression.
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Instant;

type Shared = Arc<Mutex<State>>;
type Registry = BTreeMap<PathBuf, Weak<Mutex<State>>>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
const MAX_PHASES: usize = 64;

#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Phase {
    calls: u64,
    completed: u64,
    active: u64,
    total_us: u64,
    max_us: u64,
    first_at_us: u64,
    last_at_us: u64,
}

struct State {
    started: Instant,
    phases: BTreeMap<(&'static str, &'static str), Phase>,
    omitted: u64,
    overhead_us: u64,
    errors: Vec<serde_json::Value>,
    values: BTreeMap<&'static str, u64>,
}

pub(crate) struct Session {
    root: PathBuf,
    state: Shared,
}

pub(crate) struct Measurement {
    state: Shared,
    component: &'static str,
    stage: &'static str,
    started: Instant,
}

impl Session {
    pub(crate) fn start(root: &Path) -> Self {
        let state = Arc::new(Mutex::new(State {
            started: Instant::now(),
            phases: BTreeMap::new(),
            omitted: 0,
            overhead_us: 0,
            errors: Vec::new(),
            values: BTreeMap::new(),
        }));
        let mut registry = REGISTRY.get_or_init(Mutex::default).lock().unwrap();
        registry.retain(|_, state| state.strong_count() != 0);
        assert!(registry.len() < 4, "diagnostic session bound");
        registry.insert(root.to_path_buf(), Arc::downgrade(&state));
        Self {
            root: root.to_path_buf(),
            state,
        }
    }

    pub(crate) fn report(&self, point: &'static str) {
        let state = self.state.lock().unwrap();
        let phases = state.phases.iter().map(|((component, stage), timing)| {
            serde_json::json!({"component":component,"stage":stage,"timing":timing})
        }).collect::<Vec<_>>();
        eprintln!("usage-maintenance-diagnostics {}", serde_json::json!({
            "point":point,"debugAssertions":cfg!(debug_assertions),
            "elapsedUs":micros(state.started.elapsed().as_micros()),
            "omitted":state.omitted,"probeOverheadUs":state.overhead_us,"phases":phases,
            "errors":state.errors,"values":state.values
        }));
        let _ = std::io::stderr().flush();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        REGISTRY.get().unwrap().lock().unwrap().remove(&self.root);
        self.report("session-drop");
    }
}

pub(crate) fn measure(root: &Path, component: &'static str, stage: &'static str) -> Option<Measurement> {
    begin(root, component, stage, Instant::now())
}

fn begin(root: &Path, component: &'static str, stage: &'static str, started: Instant) -> Option<Measurement> {
    let bookkeeping = Instant::now();
    let state = REGISTRY.get()?.lock().ok()?.get(root)?.upgrade()?;
    let mut locked = state.lock().unwrap();
    if !locked.phases.contains_key(&(component, stage)) && locked.phases.len() >= MAX_PHASES {
        locked.omitted += 1;
        return None;
    }
    let at = micros(started.duration_since(locked.started).as_micros());
    let phase = locked.phases.entry((component, stage)).or_default();
    if phase.calls == 0 {
        phase.first_at_us = at;
    }
    phase.calls += 1;
    phase.active += 1;
    phase.last_at_us = at;
    locked.overhead_us += micros(bookkeeping.elapsed().as_micros());
    drop(locked);
    Some(Measurement { state, component, stage, started })
}

pub(crate) fn record(root: &Path, component: &'static str, stage: &'static str, started: Instant) {
    drop(begin(root, component, stage, started));
}

pub(crate) fn error(root: &Path, component: &'static str, error: &crate::StorageError) {
    let Some(state) = REGISTRY.get().and_then(|registry| registry.lock().ok()?.get(root)?.upgrade()) else {
        return;
    };
    let mut state = state.lock().unwrap();
    if state.errors.len() < 4 {
        // This collector is registered only for the synthetic cap fixture.
        let message = error.to_string().chars().take(256).collect::<String>();
        state.errors.push(serde_json::json!({"component":component,"message":message}));
    }
}

pub(crate) fn value(root: &Path, name: &'static str, value: u64) {
    let Some(state) = REGISTRY.get().and_then(|registry| registry.lock().ok()?.get(root)?.upgrade()) else {
        return;
    };
    let mut state = state.lock().unwrap();
    if state.values.contains_key(name) || state.values.len() < 16 {
        state.values.insert(name, value);
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let at = micros(now.duration_since(state.started).as_micros());
        let elapsed = micros(now.duration_since(self.started).as_micros());
        let phase = state.phases.get_mut(&(self.component, self.stage)).unwrap();
        phase.completed += 1;
        phase.active -= 1;
        phase.total_us = phase.total_us.saturating_add(elapsed);
        phase.max_us = phase.max_us.max(elapsed);
        phase.last_at_us = at;
        state.overhead_us += micros(now.elapsed().as_micros());
    }
}

fn micros(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}
