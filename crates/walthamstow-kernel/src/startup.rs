//! Copyright (c) Scott A Dixon
//!
//! Reports an engine load on the console while it runs. Each phase logs a
//! line when it starts and when it ends, phases that visit items one at a time
//! log each item at `debug`, and a heartbeat repeats the step in hand whenever
//! the console has been quiet for the progress interval.

use crate::clock::{Moment, WallClock};
use crate::error::KernelError;
use anyhow::Result;
use std::cell::RefCell;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// The `tracing` target every startup line is logged under.
pub const TARGET: &str = "walthamstow_kernel::startup";

/// One phase of an engine load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    /// Logged as the `phase` field of every line the phase writes.
    pub id: &'static str,
    /// What the console says while the phase runs.
    pub running: &'static str,
    /// What the console says once it ends.
    pub finished: &'static str,
    /// Whether the phase visits items one at a time. Such a phase logs its
    /// start once it knows how many it will visit.
    pub itemised: bool,
}

impl Phase {
    /// A phase that does not visit items.
    pub const fn new(id: &'static str, running: &'static str, finished: &'static str) -> Self {
        Self {
            id,
            running,
            finished,
            itemised: false,
        }
    }
}

/// Receives a phase's progress through the items it visits.
pub trait ItemProgress {
    /// Announces how many items the phase will visit.
    fn expect(&self, total: usize);
    /// Starts on the named item; the one before it is done.
    fn next(&self, item: &str);
}

/// Reports nothing, for callers outside startup.
impl ItemProgress for () {
    fn expect(&self, _total: usize) {}
    fn next(&self, _item: &str) {}
}

/// What one completed phase took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRecord {
    pub phase: Phase,
    pub elapsed: Duration,
    pub counts: Vec<(&'static str, usize)>,
}

/// What one engine load did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupRecord {
    pub started_at: std::time::SystemTime,
    pub elapsed: Duration,
    pub phases: Vec<PhaseRecord>,
}

/// Runs the phases of one engine load and reports them.
pub struct StartupProgress {
    started_at: std::time::SystemTime,
    started: Moment,
    shared: Arc<Shared>,
    heartbeat: Option<JoinHandle<()>>,
    phases: Vec<PhaseRecord>,
}

impl StartupProgress {
    /// Starts a load. Its record gives its start as `clock` reads it, and its
    /// durations and its heartbeat go by the moment `now` gives.
    pub fn new(clock: &WallClock, now: impl Fn() -> Moment + Send + Sync + 'static) -> Self {
        let started = now();
        Self {
            started_at: clock.now(),
            started,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    started,
                    last_line: started,
                    step: None,
                    stopped: false,
                }),
                wake: Condvar::new(),
                now: Box::new(now),
            }),
            heartbeat: None,
            phases: Vec::new(),
        }
    }

    /// Repeats the step in hand whenever the console has been quiet for
    /// `interval`, which is longer than zero. The heartbeat logs through the
    /// subscriber and in the span of the thread that starts it.
    pub fn start_heartbeat(&mut self, interval: Duration) -> Result<()> {
        if interval.is_zero() {
            return Err(KernelError::invalid_request(
                "a startup heartbeat's interval must be longer than zero",
            )
            .into());
        }
        let shared = self.shared.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let span = tracing::Span::current();
        self.heartbeat = Some(
            std::thread::Builder::new()
                .name("startup-progress".into())
                .spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        span.in_scope(|| beat(&shared, interval))
                    });
                })?,
        );
        Ok(())
    }

    /// Runs `work` as `phase`, logging its start and, when it succeeds, its end
    /// with its duration and the counts `work` recorded.
    pub fn phase<T>(
        &mut self,
        phase: Phase,
        work: impl FnOnce(&PhaseProgress<'_>) -> Result<T>,
    ) -> Result<T> {
        let started = (self.shared.now)();
        {
            let mut state = lock(&self.shared);
            let mut step = Step::new(phase, started);
            if !phase.itemised {
                step.announce(&mut state.last_line, started);
            }
            state.step = Some(step);
        }
        let progress = PhaseProgress {
            shared: &self.shared,
            counts: RefCell::new(Vec::new()),
        };
        let result = work(&progress);
        let ended = (self.shared.now)();
        let record = PhaseRecord {
            phase,
            elapsed: ended.duration_since(started),
            counts: progress.counts.into_inner(),
        };
        let mut state = lock(&self.shared);
        let State {
            last_line, step, ..
        } = &mut *state;
        let Some(mut step) = step.take() else {
            return result;
        };
        step.finish_item(ended);
        if result.is_ok() {
            if !step.announced {
                step.announce(last_line, ended);
            }
            tracing::info!(
                name: "startup.phase.end",
                target: TARGET,
                phase = %record.phase.id,
                elapsed_s = %Seconds(record.elapsed),
                "{}{}",
                record.phase.finished,
                Counts(&record.counts),
            );
            *last_line = ended;
            drop(state);
            self.phases.push(record);
        }
        result
    }

    /// Logs the whole load, stops the heartbeat, and returns the load's record.
    pub fn finish(mut self) -> StartupRecord {
        self.stop_heartbeat();
        let record = StartupRecord {
            started_at: self.started_at,
            elapsed: (self.shared.now)().duration_since(self.started),
            phases: std::mem::take(&mut self.phases),
        };
        tracing::info!(
            name: "startup.finished",
            target: TARGET,
            elapsed_s = %Seconds(record.elapsed),
            "Loaded the engine",
        );
        record
    }

    fn stop_heartbeat(&mut self) {
        let Some(heartbeat) = self.heartbeat.take() else {
            return;
        };
        lock(&self.shared).stopped = true;
        self.shared.wake.notify_all();
        let _ = heartbeat.join();
    }
}

impl Drop for StartupProgress {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

/// The phase in hand, as `work` sees it.
pub struct PhaseProgress<'a> {
    shared: &'a Shared,
    counts: RefCell<Vec<(&'static str, usize)>>,
}

impl PhaseProgress<'_> {
    /// Records counts for the phase's end line.
    pub fn record(&self, counts: &[(&'static str, usize)]) {
        self.counts.borrow_mut().extend_from_slice(counts);
    }
}

impl ItemProgress for PhaseProgress<'_> {
    fn expect(&self, total: usize) {
        let now = (self.shared.now)();
        let mut state = lock(self.shared);
        let State {
            last_line, step, ..
        } = &mut *state;
        if let Some(step) = step {
            step.total = Some(total);
            if !step.announced {
                step.announce(last_line, now);
            }
        }
    }

    fn next(&self, item: &str) {
        let now = (self.shared.now)();
        let mut state = lock(self.shared);
        if let Some(step) = state.step.as_mut() {
            step.finish_item(now);
            step.item = Some((item.to_string(), now));
        }
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// The moment the load is at.
    now: Box<dyn Fn() -> Moment + Send + Sync>,
}

struct State {
    /// When the load started.
    started: Moment,
    last_line: Moment,
    step: Option<Step>,
    stopped: bool,
}

struct Step {
    phase: Phase,
    started: Moment,
    announced: bool,
    total: Option<usize>,
    done: usize,
    /// `done` as of the last line the heartbeat wrote.
    reported: usize,
    item: Option<(String, Moment)>,
}

impl Step {
    fn new(phase: Phase, started: Moment) -> Self {
        Self {
            phase,
            started,
            announced: false,
            total: None,
            done: 0,
            reported: 0,
            item: None,
        }
    }

    fn announce(&mut self, last_line: &mut Moment, now: Moment) {
        self.announced = true;
        match self.total {
            Some(total) => tracing::info!(
                name: "startup.phase.start",
                target: TARGET,
                phase = %self.phase.id,
                total,
                "{}",
                self.phase.running,
            ),
            None => tracing::info!(
                name: "startup.phase.start",
                target: TARGET,
                phase = %self.phase.id,
                "{}",
                self.phase.running,
            ),
        }
        *last_line = now;
    }

    fn finish_item(&mut self, now: Moment) {
        let Some((item, started)) = self.item.take() else {
            return;
        };
        self.done += 1;
        tracing::debug!(
            name: "startup.item",
            target: TARGET,
            phase = %self.phase.id,
            done = self.done,
            total = self.total.unwrap_or(self.done),
            item = %item,
            elapsed_ms = now.duration_since(started).as_millis() as u64,
            "{}",
            self.phase.running,
        );
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the heartbeat until the load stops it.
fn beat(shared: &Shared, interval: Duration) {
    let mut state = lock(shared);
    while !state.stopped {
        let wait = state.beat((shared.now)(), interval);
        state = shared
            .wake
            .wait_timeout(state, wait)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

impl State {
    /// The heartbeat's turn at `now`: logs a line when the console has been
    /// quiet for `interval`, and gives how long until a line could next be due.
    fn beat(&mut self, now: Moment, interval: Duration) -> Duration {
        let due = self.last_line + interval;
        if now < due {
            return due.duration_since(now);
        }
        self.log_beat(now);
        interval
    }

    /// Logs one heartbeat line for the step in hand: the items done since the
    /// last report, or that the step is still running.
    fn log_beat(&mut self, now: Moment) {
        let load_elapsed = now.duration_since(self.started);
        match self.step.as_mut() {
            Some(step) if step.done > step.reported => {
                step.reported = step.done;
                let item = step.item.as_ref().map_or("", |(item, _)| item.as_str());
                tracing::info!(
                    name: "startup.progress",
                    target: TARGET,
                    phase = %step.phase.id,
                    done = step.done,
                    total = step.total.unwrap_or(step.done),
                    item = %item,
                    "{}",
                    step.phase.running,
                );
            }
            Some(step) => match step.item.as_ref() {
                Some((item, started)) => tracing::info!(
                    name: "startup.still",
                    target: TARGET,
                    phase = %step.phase.id,
                    item = %item,
                    elapsed_s = %Seconds(now.duration_since(*started)),
                    "Still {}",
                    Lowercased(step.phase.running),
                ),
                None => tracing::info!(
                    name: "startup.still",
                    target: TARGET,
                    phase = %step.phase.id,
                    elapsed_s = %Seconds(now.duration_since(step.started)),
                    "Still {}",
                    Lowercased(step.phase.running),
                ),
            },
            None => tracing::info!(
                name: "startup.still",
                target: TARGET,
                elapsed_s = %Seconds(load_elapsed),
                "Still starting the engine",
            ),
        }
        self.last_line = now;
    }
}

/// Seconds to one decimal place.
struct Seconds(Duration);

impl fmt::Display for Seconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}", self.0.as_secs_f64())
    }
}

/// ` name=count` for each recorded count.
struct Counts<'a>(&'a [(&'static str, usize)]);

impl fmt::Display for Counts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, count) in self.0 {
            write!(f, " {name}={count}")?;
        }
        Ok(())
    }
}

/// A phase's running text with its first letter lowercased, to follow "Still".
struct Lowercased(&'static str);

impl fmt::Display for Lowercased {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut chars = self.0.chars();
        if let Some(first) = chars.next() {
            write!(f, "{}", first.to_lowercase())?;
        }
        f.write_str(chars.as_str())
    }
}

/// Captures the startup lines a piece of work logs, on any thread.
#[cfg(any(test, feature = "capture"))]
pub mod capture {
    use super::TARGET;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, Once};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    /// One startup line: its event name and its fields as text.
    #[derive(Debug, Clone)]
    pub struct Captured {
        pub name: &'static str,
        pub fields: BTreeMap<&'static str, String>,
    }

    impl Captured {
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }
    }

    const CAPTURE_SPAN: &str = "startup_capture";

    static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(0);
    static CAPTURES: LazyLock<Mutex<HashMap<u64, Vec<Captured>>>> = LazyLock::new(Default::default);

    /// The capture a span belongs to, kept in the span's extensions.
    struct CaptureId(u64);

    /// The process's one subscriber. It enables only startup lines and files
    /// each under the capture whose span it was logged in. A subscriber
    /// scoped to the capturing thread is not enough: while it is the only one
    /// registered, tracing caches a line's interest from whichever thread logs
    /// it first, and a load on another test's thread turns the line off for
    /// every capture.
    struct Router;

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Router {
        fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
            if metadata.target() == TARGET {
                Interest::always()
            } else {
                Interest::never()
            }
        }

        fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
            if attributes.metadata().name() != CAPTURE_SPAN {
                return;
            }
            let mut fields = Fields::default();
            attributes.record(&mut fields);
            let capture = fields.0["capture"].parse().expect("capture id");
            if let Some(span) = context.span(id) {
                span.extensions_mut().insert(CaptureId(capture));
            }
        }

        fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
            let Some(scope) = context.event_scope(event) else {
                return;
            };
            let enclosing = scope
                .filter_map(|span| span.extensions().get::<CaptureId>().map(|id| id.0))
                .collect::<Vec<_>>();
            if enclosing.is_empty() {
                return;
            }
            let mut fields = Fields::default();
            event.record(&mut fields);
            let line = Captured {
                name: event.metadata().name(),
                fields: fields.0,
            };
            let mut captures = CAPTURES.lock().expect("captures");
            for capture in enclosing {
                if let Some(lines) = captures.get_mut(&capture) {
                    lines.push(line.clone());
                }
            }
        }
    }

    #[derive(Default)]
    struct Fields(BTreeMap<&'static str, String>);

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }
    }

    /// Runs `work` and returns what it produced with every startup line it
    /// logged, at every level. A line logged inside a capture that `work`
    /// itself runs belongs to both captures.
    pub fn startup_lines<T>(work: impl FnOnce() -> T) -> (T, Vec<Captured>) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Router))
                .expect("the startup capture is the process's only subscriber");
        });
        let capture = NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed);
        CAPTURES
            .lock()
            .expect("captures")
            .insert(capture, Vec::new());
        let result = tracing::info_span!(target: TARGET, CAPTURE_SPAN, capture).in_scope(work);
        let lines = CAPTURES
            .lock()
            .expect("captures")
            .remove(&capture)
            .expect("capture");
        (result, lines)
    }
}

#[cfg(test)]
mod tests {
    use super::capture::{startup_lines, Captured};
    use super::*;

    const LOADING: Phase = Phase::new("loading", "Loading", "Loaded");
    const VISITING: Phase = Phase {
        itemised: true,
        ..Phase::new("visiting", "Visiting items", "Visited items")
    };
    const INTERVAL: Duration = Duration::from_secs(2);

    /// A clock that moves only when a test moves it.
    #[derive(Clone)]
    struct Hands(Arc<Mutex<Moment>>);

    impl Hands {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Moment::ORIGIN)))
        }

        fn advance(&self, by: Duration) {
            let mut now = self.0.lock().expect("hands");
            *now = *now + by;
        }

        /// A load whose durations and heartbeat go by these hands.
        fn load(&self) -> StartupProgress {
            let hands = self.clone();
            StartupProgress::new(
                &WallClock::new(|| std::time::SystemTime::UNIX_EPOCH),
                move || *hands.0.lock().expect("hands"),
            )
        }
    }

    /// Gives the heartbeat its turn, as its thread does when its wait ends,
    /// and returns how long it would wait next.
    fn heartbeat(phase: &PhaseProgress<'_>) -> Duration {
        let now = (phase.shared.now)();
        lock(phase.shared).beat(now, INTERVAL)
    }

    fn named<'a>(lines: &'a [Captured], name: &str) -> Vec<&'a Captured> {
        lines.iter().filter(|line| line.name == name).collect()
    }

    /// A capture keeps the lines its work logs even when another thread, with
    /// no capture of its own, logged the same line first.
    #[test]
    fn a_line_another_thread_logged_first_is_still_captured() {
        fn line() {
            tracing::info!(name: "startup.capture_probe", target: TARGET, "Probed the capture");
        }
        let ((), lines) = startup_lines(|| {
            std::thread::spawn(line).join().expect("uncaptured thread");
            line();
        });
        assert_eq!(named(&lines, "startup.capture_probe").len(), 1);
    }

    /// A capture that runs inside another keeps its own lines, and the outer
    /// capture keeps them as well as its own.
    #[test]
    fn a_capture_inside_another_keeps_its_lines() {
        let (inner, outer) = startup_lines(|| {
            tracing::info!(name: "startup.outer_probe", target: TARGET, "Probed the outer capture");
            let ((), inner) = startup_lines(|| {
                tracing::info!(name: "startup.inner_probe", target: TARGET, "Probed the inner capture");
            });
            inner
        });
        let names = |lines: &[Captured]| lines.iter().map(|line| line.name).collect::<Vec<_>>();
        assert_eq!(names(&inner), vec!["startup.inner_probe"]);
        assert_eq!(
            names(&outer),
            vec!["startup.outer_probe", "startup.inner_probe"]
        );
    }

    /// A load's record gives its start as the wall clock it was given reads it.
    #[test]
    fn a_load_starts_when_its_clock_says() {
        let start = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let record =
            StartupProgress::new(&WallClock::new(move || start), || Moment::ORIGIN).finish();

        assert_eq!(record.started_at, start);
    }

    /// A phase and its load last as long as the load's clock moved.
    #[test]
    fn a_phase_and_its_load_last_as_long_as_the_clock_moved() {
        let hands = Hands::new();
        let (within, after) = (Duration::from_secs(3), Duration::from_secs(4));
        let mut progress = hands.load();
        progress
            .phase(LOADING, |_| {
                hands.advance(within);
                Ok(())
            })
            .expect("phase");
        hands.advance(after);
        let record = progress.finish();

        assert_eq!(record.phases[0].elapsed, within);
        assert_eq!(record.elapsed, within + after);
    }

    /// The heartbeat says nothing until the console has been quiet for the
    /// interval, and waits out what is left of it.
    #[test]
    fn the_heartbeat_waits_until_the_console_has_been_quiet_for_the_interval() {
        let hands = Hands::new();
        let early = INTERVAL / 2;
        let ((), lines) = startup_lines(|| {
            let mut progress = hands.load();
            progress
                .phase(LOADING, |phase| {
                    hands.advance(early);
                    assert_eq!(heartbeat(phase), INTERVAL - early);
                    hands.advance(INTERVAL - early);
                    assert_eq!(heartbeat(phase), INTERVAL);
                    Ok(())
                })
                .expect("phase");
            progress.finish();
        });

        assert_eq!(named(&lines, "startup.still").len(), 1);
    }

    /// One large item must not leave the console silent: while a step runs
    /// past the interval, the heartbeat keeps naming it.
    #[test]
    fn the_heartbeat_repeats_a_step_that_outlasts_the_interval() {
        let hands = Hands::new();
        let item = "a-large-item";
        let beats = 2;
        let ((), lines) = startup_lines(|| {
            let mut progress = hands.load();
            progress
                .phase(VISITING, |phase| {
                    phase.expect(1);
                    phase.next(item);
                    for _ in 0..beats {
                        hands.advance(INTERVAL);
                        heartbeat(phase);
                    }
                    Ok(())
                })
                .expect("phase");
            progress.finish();
        });

        let still = named(&lines, "startup.still");
        assert_eq!(still.len(), beats, "{lines:#?}");
        assert!(still.iter().all(
            |line| line.field("item") == Some(item) && line.field("phase") == Some(VISITING.id)
        ));
    }

    /// Items that each finish within the interval are reported by count.
    #[test]
    fn the_heartbeat_reports_items_done_out_of_the_total() {
        let hands = Hands::new();
        let total = 6;
        let ((), lines) = startup_lines(|| {
            let mut progress = hands.load();
            progress
                .phase(VISITING, |phase| {
                    phase.expect(total);
                    for index in 0..total {
                        phase.next(&format!("item-{index}"));
                        hands.advance(INTERVAL);
                        heartbeat(phase);
                    }
                    Ok(())
                })
                .expect("phase");
            progress.finish();
        });

        let reported = named(&lines, "startup.progress")
            .into_iter()
            .map(|line| {
                let done: usize = line.field("done").expect("done").parse().expect("count");
                let of: usize = line.field("total").expect("total").parse().expect("count");
                (done, of)
            })
            .collect::<Vec<_>>();
        assert!(!reported.is_empty(), "{lines:#?}");
        assert!(reported
            .iter()
            .all(|(done, of)| *of == total && (1..=total).contains(done)));
        assert!(reported.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    /// A heartbeat with no interval would never wait, so it is refused.
    #[test]
    fn a_heartbeat_with_no_interval_is_refused() {
        let error = Hands::new()
            .load()
            .start_heartbeat(Duration::ZERO)
            .expect_err("no interval");

        assert!(error.downcast_ref::<KernelError>().is_some());
    }

    /// A phase that fails reports no end, and the error reaches the caller.
    #[test]
    fn a_failed_phase_reports_its_start_and_no_end() {
        let (result, lines) = startup_lines(|| {
            let mut progress = Hands::new().load();
            progress.phase(LOADING, |_| -> Result<()> {
                Err(anyhow::anyhow!("the load cannot proceed"))
            })
        });

        assert!(result.is_err());
        let names = lines.iter().map(|line| line.name).collect::<Vec<_>>();
        assert_eq!(names, vec!["startup.phase.start"]);
    }
}
