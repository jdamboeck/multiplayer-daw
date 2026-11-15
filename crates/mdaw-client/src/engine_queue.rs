//! The queue between the threads that ask the audio engine for work (the UI thread, the network
//! thread handing over downloads, the plugin scan) and the one worker that carries it out.
//!
//! A plain channel runs everything in the order it was asked for, so a click on "Open window"
//! waits behind every settings read and project sync queued before it, and requests that repeat
//! (reading the settings of a plugin whose window is open, twice a second) pile up without bound
//! when one takes longer than the pace they come at. Here every job says how urgent it is, what
//! it touches and whether a newer request may take its place, and the worker picks the next job
//! by that:
//!
//! - Interactive jobs (a window opened, a knob turned, the transport) go first, but never past
//!   an earlier job that touches the same thing: an editor waits for the sync that creates its
//!   device, a parameter for the reading of its plugin's settings before it. Global jobs (a
//!   sync, a render) touch everything. Once the oldest Normal job has waited a second, plain
//!   order rules again, so a stream of clicks cannot hold back the project for long.
//! - Normal jobs (syncs, settings reads, downloads, renders) run in the order they were asked
//!   for, Interactive ones included.
//! - Background jobs (looking at a plugin's settings, reading parameters for the device tab) run
//!   only when nothing else waits. There is at most one per key in the queue, and one more to
//!   follow if a request comes while the same key runs: however often they are asked for, the
//!   queue stays short.
//!
//! No job is stopped once started; this only chooses which starts next. Producers take the lock
//! for a moment and never wait for the worker. The module knows nothing of the engine, so it is
//! tested on its own.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Debug;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How urgent a job is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Class {
    Interactive,
    Normal,
    Background,
}

/// What an ordered job touches, for which jobs may not overtake it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    /// The whole project (syncs, downloads, renders).
    Global,
    /// One device (by its raw id).
    Device(u64),
    /// The transport and the output.
    Transport,
}

impl Key {
    /// Whether two jobs touching these may not change order.
    pub fn conflicts(self, other: Key) -> bool {
        match (self, other) {
            (Key::Global, _) | (_, Key::Global) => true,
            (Key::Device(a), Key::Device(b)) => a == b,
            (Key::Transport, Key::Transport) => true,
            _ => false,
        }
    }
}

/// What a newer request does to an older one waiting in the queue.
#[derive(Clone, PartialEq, Debug)]
enum Coalesce<C> {
    None,
    /// Takes the place of a queued job with this key, unless a job queued after that one
    /// conflicts with it (then it would run before something it used to run after).
    Replace(C),
    /// Takes the place of a queued job with this key wherever it is.
    ReplaceAlways(C),
    /// Is dropped if a job with this key is queued.
    Unique(C),
    /// A background job: at most one queued, and one to follow a running one.
    Background(C),
}

impl<C> Coalesce<C> {
    fn key(&self) -> Option<&C> {
        match self {
            Coalesce::None => None,
            Coalesce::Replace(c)
            | Coalesce::ReplaceAlways(c)
            | Coalesce::Unique(c)
            | Coalesce::Background(c) => Some(c),
        }
    }
}

/// How a job is to be queued.
#[derive(Clone, Debug)]
pub struct Spec<C> {
    class: Class,
    key: Key,
    coalesce: Coalesce<C>,
    label: &'static str,
}

impl<C> Spec<C> {
    /// An Interactive or Normal job touching `key`; `label` names it in statistics and traces.
    pub fn ordered(class: Class, key: Key, label: &'static str) -> Self {
        debug_assert!(
            class != Class::Background,
            "background jobs have a spec of their own"
        );
        Self {
            class,
            key,
            coalesce: Coalesce::None,
            label,
        }
    }

    /// A background job, one per `key` (see the module doc).
    pub fn background(key: C, label: &'static str) -> Self {
        Self {
            class: Class::Background,
            key: Key::Global,
            coalesce: Coalesce::Background(key),
            label,
        }
    }

    /// Takes the place of a queued job with the same `key` if nothing queued after it conflicts.
    pub fn replace(mut self, key: C) -> Self {
        self.coalesce = Coalesce::Replace(key);
        self
    }

    /// Takes the place of a queued job with the same `key`, wherever it waits.
    pub fn replace_always(mut self, key: C) -> Self {
        self.coalesce = Coalesce::ReplaceAlways(key);
        self
    }

    /// Dropped when a job with the same `key` is queued already.
    pub fn unique(mut self, key: C) -> Self {
        self.coalesce = Coalesce::Unique(key);
        self
    }

    pub fn class(&self) -> Class {
        self.class
    }
}

/// What became of a submitted job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Submitted {
    Queued,
    /// Merged into a job waiting already (it replaced it, or the same was queued).
    Coalesced,
    /// To run after the running job of its key.
    FollowUp,
    /// Not wanted: one already follows the running job of its key.
    Dropped,
    /// The queue is closed.
    Closed,
}

/// Jobs waiting, per class.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Depths {
    pub interactive: usize,
    pub normal: usize,
    pub background: usize,
}

impl Depths {
    fn max(self, other: Depths) -> Depths {
        Depths {
            interactive: self.interactive.max(other.interactive),
            normal: self.normal.max(other.normal),
            background: self.background.max(other.background),
        }
    }
}

/// Counts and times per label.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct LabelStats {
    pub submitted: u64,
    pub coalesced: u64,
    /// Not run: a second follow-up, cancelled, or left when the queue closed.
    pub dropped: u64,
    pub ran: u64,
    pub wait_total: Duration,
    pub wait_max: Duration,
    pub run_total: Duration,
    pub run_max: Duration,
}

/// What the queue has seen, for tests and traces.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    pub depths: Depths,
    pub max_depths: Depths,
    pub labels: BTreeMap<&'static str, LabelStats>,
}

/// A job taken by the worker, with what it is.
#[derive(Clone, Copy, Debug)]
pub struct JobInfo {
    pub label: &'static str,
    pub class: Class,
    pub enqueued: Instant,
    pub picked: Instant,
    /// What waited when it was taken (itself not counted).
    pub depths: Depths,
}

impl JobInfo {
    pub fn waited(&self) -> Duration {
        self.picked.saturating_duration_since(self.enqueued)
    }
}

/// A job for the worker. Hand its `info` back to [`EngineQueue::done`] when it has run.
pub struct Picked<J> {
    pub payload: J,
    pub info: JobInfo,
}

struct Entry<J, C> {
    payload: J,
    class: Class,
    key: Key,
    coalesce: Coalesce<C>,
    label: &'static str,
    enqueued: Instant,
}

struct State<J, C> {
    /// Interactive and Normal jobs in the order they were asked for.
    ordered: VecDeque<Entry<J, C>>,
    background: VecDeque<Entry<J, C>>,
    /// The key of the background job running now.
    running: Option<C>,
    /// One more of the running background job's key, asked for while it ran.
    follow_up: Option<Entry<J, C>>,
    closed: bool,
    stats: Stats,
}

impl<J, C> State<J, C> {
    fn depths(&self) -> Depths {
        let interactive = self
            .ordered
            .iter()
            .filter(|e| e.class == Class::Interactive)
            .count();
        Depths {
            interactive,
            normal: self.ordered.len() - interactive,
            background: self.background.len(),
        }
    }

    fn measure(&mut self) {
        let depths = self.depths();
        self.stats.depths = depths;
        self.stats.max_depths = self.stats.max_depths.max(depths);
    }

    fn label(&mut self, label: &'static str) -> &mut LabelStats {
        self.stats.labels.entry(label).or_default()
    }
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The queue; share it in an `Arc`.
pub struct EngineQueue<J, C> {
    state: Mutex<State<J, C>>,
    ready: Condvar,
    clock: Clock,
    /// How long the oldest Normal job may wait before Interactive ones stop overtaking.
    aging: Duration,
}

/// How long the oldest Normal job waits at most before plain order rules again.
pub const AGING: Duration = Duration::from_secs(1);

impl<J, C: Clone + PartialEq + Debug> Default for EngineQueue<J, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J, C: Clone + PartialEq + Debug> EngineQueue<J, C> {
    pub fn new() -> Self {
        Self::with_clock(AGING, Arc::new(Instant::now))
    }

    /// With another aging limit and clock, for tests.
    pub fn with_clock(aging: Duration, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self {
            state: Mutex::new(State {
                ordered: VecDeque::new(),
                background: VecDeque::new(),
                running: None,
                follow_up: None,
                closed: false,
                stats: Stats::default(),
            }),
            ready: Condvar::new(),
            clock,
            aging,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<J, C>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a job, or merges it into one waiting. Never waits for the worker.
    pub fn submit(&self, spec: Spec<C>, payload: J) -> Submitted {
        let now = (self.clock)();
        let mut state = self.lock();
        if state.closed {
            return Submitted::Closed;
        }
        state.label(spec.label).submitted += 1;
        let entry = Entry {
            payload,
            class: spec.class,
            key: spec.key,
            coalesce: spec.coalesce,
            label: spec.label,
            enqueued: now,
        };
        let key = entry.coalesce.key().cloned();
        let same = |e: &Entry<J, C>| key.is_some() && e.coalesce.key() == key.as_ref();
        let outcome = match &entry.coalesce {
            Coalesce::None => None,
            Coalesce::Replace(_) => match state.ordered.iter().rposition(same) {
                Some(at)
                    if !state
                        .ordered
                        .iter()
                        .skip(at + 1)
                        .any(|later| later.key.conflicts(state.ordered[at].key)) =>
                {
                    Some((at, true))
                }
                _ => None,
            },
            Coalesce::ReplaceAlways(_) => state.ordered.iter().rposition(same).map(|at| (at, true)),
            Coalesce::Unique(_) => state.ordered.iter().position(same).map(|at| (at, false)),
            Coalesce::Background(_) => {
                if state.background.iter().any(same) {
                    state.label(entry.label).coalesced += 1;
                    return Submitted::Coalesced;
                }
                if state.running.is_some() && state.running == key {
                    if state.follow_up.is_some() {
                        state.label(entry.label).dropped += 1;
                        return Submitted::Dropped;
                    }
                    state.follow_up = Some(entry);
                    return Submitted::FollowUp;
                }
                state.background.push_back(entry);
                state.measure();
                drop(state);
                self.ready.notify_one();
                return Submitted::Queued;
            }
        };
        match outcome {
            Some((at, replace)) => {
                state.label(entry.label).coalesced += 1;
                if replace {
                    // The new request's payload, in the old one's place and with its waiting time.
                    state.ordered[at].payload = entry.payload;
                }
                Submitted::Coalesced
            }
            None => {
                state.ordered.push_back(entry);
                state.measure();
                drop(state);
                self.ready.notify_one();
                Submitted::Queued
            }
        }
    }

    /// Drops the queued background jobs whose key `matches`, and a follow-up of one.
    pub fn cancel_background(&self, matches: impl Fn(&C) -> bool) {
        let mut state = self.lock();
        let mut dropped = Vec::new();
        let mut kept = VecDeque::new();
        for entry in std::mem::take(&mut state.background) {
            if entry.coalesce.key().is_some_and(&matches) {
                dropped.push(entry.label);
            } else {
                kept.push_back(entry);
            }
        }
        state.background = kept;
        if state
            .follow_up
            .as_ref()
            .is_some_and(|e| e.coalesce.key().is_some_and(&matches))
            && let Some(entry) = state.follow_up.take()
        {
            dropped.push(entry.label);
        }
        for label in dropped {
            state.label(label).dropped += 1;
        }
        state.measure();
    }

    /// The next job, waiting for one; `None` once the queue is closed.
    pub fn next(&self) -> Option<Picked<J>> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(picked) = self.pick(&mut state) {
                return Some(picked);
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The next job if there is one, without waiting.
    pub fn try_next(&self) -> Option<Picked<J>> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        self.pick(&mut state)
    }

    /// The oldest background job, whatever else waits (for a flush, which runs them first).
    pub fn next_background(&self) -> Option<Picked<J>> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let entry = state.background.pop_front()?;
        Some(self.take(&mut state, entry))
    }

    /// How many background jobs wait.
    pub fn background_len(&self) -> usize {
        self.lock().background.len()
    }

    fn pick(&self, state: &mut State<J, C>) -> Option<Picked<J>> {
        let now = (self.clock)();
        let aged = state
            .ordered
            .iter()
            .find(|e| e.class == Class::Normal)
            .is_some_and(|e| now.saturating_duration_since(e.enqueued) >= self.aging);
        let mut at = None;
        if !aged {
            at = (0..state.ordered.len()).find(|&i| {
                let e = &state.ordered[i];
                e.class == Class::Interactive
                    && !state
                        .ordered
                        .iter()
                        .take(i)
                        .any(|before| before.key.conflicts(e.key))
            });
        }
        let entry = match at {
            Some(i) => state.ordered.remove(i),
            None => state.ordered.pop_front(),
        };
        let entry = match entry {
            Some(entry) => entry,
            None => state.background.pop_front()?,
        };
        Some(self.take(state, entry))
    }

    fn take(&self, state: &mut State<J, C>, entry: Entry<J, C>) -> Picked<J> {
        let now = (self.clock)();
        if entry.class == Class::Background {
            state.running = entry.coalesce.key().cloned();
        }
        state.measure();
        let info = JobInfo {
            label: entry.label,
            class: entry.class,
            enqueued: entry.enqueued,
            picked: now,
            depths: state.stats.depths,
        };
        let waited = info.waited();
        let stats = state.label(entry.label);
        stats.wait_total += waited;
        stats.wait_max = stats.wait_max.max(waited);
        Picked {
            payload: entry.payload,
            info,
        }
    }

    /// The worker is done with a job; queues what was to follow it. Returns how long it ran.
    pub fn done(&self, info: &JobInfo) -> Duration {
        let ran = (self.clock)().saturating_duration_since(info.picked);
        let mut state = self.lock();
        let stats = state.label(info.label);
        stats.ran += 1;
        stats.run_total += ran;
        stats.run_max = stats.run_max.max(ran);
        if info.class == Class::Background {
            state.running = None;
            if let Some(entry) = state.follow_up.take() {
                state.background.push_back(entry);
                state.measure();
                drop(state);
                self.ready.notify_one();
            }
        }
        ran
    }

    /// Ends the queue: the worker's [`EngineQueue::next`] returns `None`, jobs still waiting are
    /// dropped (a reply channel among them is dropped too, which its caller sees), and later
    /// submissions are ignored.
    pub fn close(&self) {
        let left = {
            let mut state = self.lock();
            state.closed = true;
            let mut left: Vec<Entry<J, C>> = state.ordered.drain(..).collect();
            left.extend(state.background.drain(..));
            left.extend(state.follow_up.take());
            for entry in &left {
                state.label(entry.label).dropped += 1;
            }
            state.measure();
            left
        };
        self.ready.notify_all();
        drop(left);
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats.clone()
    }

    pub fn depths(&self) -> Depths {
        self.lock().stats.depths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone, PartialEq, Debug)]
    enum K {
        Param(u64, u32),
        Editor(u64),
        Capture(u64),
        Look(u64),
        Params(u64),
    }

    /// A clock that moves only when told to.
    fn manual() -> (Arc<AtomicU64>, Clock) {
        let base = Instant::now();
        let millis = Arc::new(AtomicU64::new(0));
        let read = millis.clone();
        (
            millis,
            Arc::new(move || base + Duration::from_millis(read.load(Ordering::SeqCst))),
        )
    }

    fn queue() -> (Arc<AtomicU64>, EngineQueue<&'static str, K>) {
        let (millis, clock) = manual();
        (millis, EngineQueue::with_clock(AGING, clock))
    }

    fn interactive(key: Key) -> Spec<K> {
        Spec::ordered(Class::Interactive, key, "interactive")
    }

    fn normal(key: Key) -> Spec<K> {
        Spec::ordered(Class::Normal, key, "normal")
    }

    /// Takes and finishes every job, in the order the worker gets them.
    fn drain(q: &EngineQueue<&'static str, K>) -> Vec<&'static str> {
        let mut order = Vec::new();
        while let Some(job) = q.try_next() {
            order.push(job.payload);
            q.done(&job.info);
        }
        order
    }

    #[test]
    fn ordered_jobs_run_in_order() {
        let (_, q) = queue();
        q.submit(normal(Key::Global), "sync");
        q.submit(normal(Key::Device(1)), "capture");
        q.submit(normal(Key::Global), "blob");
        assert_eq!(drain(&q), ["sync", "capture", "blob"]);
    }

    #[test]
    fn interactive_overtakes_what_it_does_not_touch() {
        let (_, q) = queue();
        q.submit(normal(Key::Device(1)), "capture 1");
        q.submit(normal(Key::Device(2)), "retry 2");
        q.submit(interactive(Key::Device(3)), "editor 3");
        q.submit(interactive(Key::Transport), "play");
        assert_eq!(drain(&q), ["editor 3", "play", "capture 1", "retry 2"]);
    }

    #[test]
    fn interactive_waits_for_what_it_touches() {
        let (_, q) = queue();
        // The editor waits for the sync that creates its device.
        q.submit(normal(Key::Global), "sync");
        q.submit(interactive(Key::Device(1)), "editor 1");
        assert_eq!(drain(&q), ["sync", "editor 1"]);
        // A parameter waits for the settings read of its device, not of another one.
        q.submit(normal(Key::Device(1)), "capture 1");
        q.submit(normal(Key::Device(2)), "retry 2");
        q.submit(interactive(Key::Device(2)), "set 2");
        q.submit(interactive(Key::Device(3)), "set 3");
        assert_eq!(drain(&q), ["set 3", "capture 1", "retry 2", "set 2"]);
        // A seek touches the transport only, but a sync touches everything: it waits too.
        q.submit(normal(Key::Global), "sync");
        q.submit(interactive(Key::Transport), "seek");
        assert_eq!(drain(&q), ["sync", "seek"]);
        // Interactive jobs that touch the same thing keep their order.
        q.submit(normal(Key::Device(9)), "capture 9");
        q.submit(interactive(Key::Transport), "play");
        q.submit(interactive(Key::Transport), "seek");
        assert_eq!(drain(&q), ["play", "seek", "capture 9"]);
    }

    #[test]
    fn a_normal_job_waiting_a_second_stops_overtaking() {
        let (millis, q) = queue();
        q.submit(normal(Key::Device(1)), "capture");
        q.submit(interactive(Key::Device(2)), "editor");
        millis.store(999, Ordering::SeqCst);
        let first = q.try_next().unwrap();
        assert_eq!(first.payload, "editor", "999 ms: still overtakes");
        q.done(&first.info);
        q.submit(interactive(Key::Device(2)), "editor again");
        millis.store(1000, Ordering::SeqCst);
        assert_eq!(drain(&q), ["capture", "editor again"], "a second: plain order");
    }

    #[test]
    fn background_runs_only_when_nothing_else_waits() {
        let (_, q) = queue();
        q.submit(Spec::background(K::Look(1), "look"), "look 1");
        q.submit(normal(Key::Device(3)), "capture");
        q.submit(interactive(Key::Device(4)), "set");
        assert_eq!(drain(&q), ["set", "capture", "look 1"]);
    }

    #[test]
    fn repeated_background_requests_stay_bounded() {
        let (_, q) = queue();
        q.submit(Spec::background(K::Look(1), "look"), "look");
        let running = q.try_next().unwrap();
        let mut outcomes = Vec::new();
        for _ in 0..1000 {
            outcomes.push(q.submit(Spec::background(K::Look(1), "look"), "look"));
        }
        assert_eq!(outcomes[0], Submitted::FollowUp);
        assert!(outcomes[1..].iter().all(|o| *o == Submitted::Dropped));
        assert_eq!(
            q.depths().background,
            0,
            "the follow-up waits for the running one"
        );
        q.done(&running.info);
        assert_eq!(q.depths().background, 1);
        assert_eq!(drain(&q), ["look"]);
        assert!(q.stats().max_depths.background <= 1);

        for i in 0..1000u64 {
            q.submit(Spec::background(K::Params(i % 3), "params"), "params");
        }
        assert_eq!(q.depths().background, 3);
        assert_eq!(q.stats().labels["params"].coalesced, 997);
    }

    #[test]
    fn a_parameter_set_twice_is_set_once_to_the_last_value() {
        let (_, q) = queue();
        q.submit(normal(Key::Device(1)), "capture");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.1");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.2");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.3");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 7)), "other param");
        assert_eq!(drain(&q), ["capture", "0.3", "other param"]);
        // Not across a job of the same device queued after it (another parameter's set too):
        // the order stays.
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.4");
        q.submit(normal(Key::Device(1)).unique(K::Capture(1)), "capture");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.5");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 7)), "other");
        q.submit(interactive(Key::Device(1)).replace(K::Param(1, 0)), "0.6");
        assert_eq!(drain(&q), ["0.4", "capture", "0.5", "other", "0.6"]);
    }

    #[test]
    fn the_last_editor_request_wins() {
        let (_, q) = queue();
        q.submit(normal(Key::Global), "sync");
        q.submit(interactive(Key::Device(1)).replace_always(K::Editor(1)), "open");
        q.submit(normal(Key::Global), "sync 2");
        q.submit(interactive(Key::Device(1)).replace_always(K::Editor(1)), "close");
        assert_eq!(drain(&q), ["sync", "close", "sync 2"]);
    }

    #[test]
    fn unique_jobs_are_not_queued_twice() {
        let (_, q) = queue();
        assert_eq!(
            q.submit(normal(Key::Device(1)).unique(K::Capture(1)), "a"),
            Submitted::Queued
        );
        assert_eq!(
            q.submit(normal(Key::Device(1)).unique(K::Capture(1)), "b"),
            Submitted::Coalesced
        );
        q.submit(normal(Key::Device(2)).unique(K::Capture(2)), "c");
        assert_eq!(drain(&q), ["a", "c"]);
    }

    #[test]
    fn closing_a_window_cancels_its_background_jobs() {
        let (_, q) = queue();
        q.submit(Spec::background(K::Look(1), "look"), "look 1");
        q.submit(Spec::background(K::Params(1), "params"), "params 1");
        q.submit(Spec::background(K::Look(2), "look"), "look 2");
        let running = q.try_next().unwrap();
        assert_eq!(running.payload, "look 1");
        q.submit(Spec::background(K::Look(1), "look"), "follow-up");
        q.cancel_background(|k| matches!(k, K::Look(1) | K::Params(1)));
        q.done(&running.info);
        assert_eq!(drain(&q), ["look 2"]);
    }

    #[test]
    fn a_flush_takes_the_background_jobs_first() {
        let (_, q) = queue();
        q.submit(Spec::background(K::Look(1), "look"), "look");
        q.submit(normal(Key::Global), "flush");
        let flush = q.try_next().unwrap();
        assert_eq!(flush.payload, "flush");
        let n = q.background_len();
        let mut ran = Vec::new();
        for _ in 0..n {
            let job = q.next_background().unwrap();
            ran.push(job.payload);
            q.done(&job.info);
        }
        q.done(&flush.info);
        assert_eq!(ran, ["look"]);
        assert!(q.try_next().is_none());
    }

    #[test]
    fn producers_never_wait_for_the_worker() {
        let q = Arc::new(EngineQueue::<&'static str, K>::new());
        q.submit(normal(Key::Global), "long");
        let (held, release) = std::sync::mpsc::channel::<()>();
        let worker = {
            let q = q.clone();
            std::thread::spawn(move || {
                let job = q.next().unwrap();
                held.send(()).unwrap();
                // Holds the job a while, as a slow engine call would.
                std::thread::sleep(Duration::from_millis(300));
                q.done(&job.info);
            })
        };
        release.recv().unwrap();
        let t = Instant::now();
        for _ in 0..100 {
            q.submit(normal(Key::Global), "more");
        }
        assert!(t.elapsed() < Duration::from_millis(100), "{:?}", t.elapsed());
        worker.join().unwrap();
    }

    #[test]
    fn closing_wakes_a_waiting_worker() {
        let q = Arc::new(EngineQueue::<&'static str, K>::new());
        let worker = {
            let q = q.clone();
            std::thread::spawn(move || q.next().map(|j| j.payload))
        };
        std::thread::sleep(Duration::from_millis(50));
        q.submit(normal(Key::Global), "left");
        let first = worker.join().unwrap();
        assert_eq!(first, Some("left"));
        let worker = {
            let q = q.clone();
            std::thread::spawn(move || q.next().map(|j| j.payload))
        };
        std::thread::sleep(Duration::from_millis(50));
        q.close();
        assert_eq!(worker.join().unwrap(), None);
        assert_eq!(q.submit(normal(Key::Global), "late"), Submitted::Closed);
    }

    #[test]
    fn closing_drops_what_waits() {
        let q = EngineQueue::<std::sync::mpsc::Sender<()>, K>::new();
        let (tx, rx) = std::sync::mpsc::channel();
        q.submit(normal(Key::Global), tx);
        q.close();
        assert!(rx.recv().is_err(), "the reply's sender is gone");
    }
}
