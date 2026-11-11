//! The DAW's side: plugins found and run by helper processes.

use super::protocol::{self, Reply, Request};
use super::shm::{self, EVENTS, Event, NOTE_OFF, NOTE_ON, Shared};
use crate::clap::plugin_files;
use crate::{
    Backend, Creation, Instance, ParamInfo, PluginError, PluginFormat, PluginInfo, Settings, Waker, error,
};
use mdaw_dsp::{BlockTime, ExternalDevice, MAX_BLOCK};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// How long a helper may take to answer the main thread before it counts as hung.
const ANSWER: Duration = Duration::from_secs(5);
/// The same for what can take long with a large plugin: starting, and its state.
const LONG_ANSWER: Duration = Duration::from_secs(30);
/// How long scanning one file may take.
const SCAN: Duration = Duration::from_secs(20);
/// What a helper gets for a block on top of the block's own length, for waking up.
const SLACK: Duration = Duration::from_millis(2);
/// How long an offline render waits for a block.
const OFFLINE_WAIT: Duration = Duration::from_secs(10);
/// A helper whose audio has not come back for this long has hung there.
const HUNG_MICROS: i64 = 2_000_000;

/// CLAP plugins, each run in a helper process of its own: a plugin that crashes takes only
/// its helper down.
pub struct IsolatedBackend {
    helper: PathBuf,
    /// The plugin files to look at: `None` for the usual places.
    files: Option<Vec<PathBuf>>,
    builtin: bool,
    /// What was found, with the file it is in (`None`: built in).
    found: Vec<(PluginInfo, Option<PathBuf>)>,
    waker: Option<Waker>,
}

impl IsolatedBackend {
    /// The plugins installed in the usual places, and the test synth if `builtin`.
    pub fn standard(helper: PathBuf, builtin: bool) -> Self {
        Self {
            helper,
            files: None,
            builtin,
            found: Vec::new(),
            waker: None,
        }
    }

    pub fn test_synth_only(helper: PathBuf) -> Self {
        Self {
            helper,
            files: Some(Vec::new()),
            builtin: true,
            found: Vec::new(),
            waker: None,
        }
    }

    /// Asks a helper what `file` holds (`None`: the test synth). A file whose plugin crashes the
    /// helper is passed over.
    fn scan_one(&self, file: Option<&Path>) -> Vec<PluginInfo> {
        let mut command = Command::new(&self.helper);
        command.arg("scan");
        match file {
            Some(file) => command.arg(file),
            None => command.arg("builtin"),
        };
        let Ok(mut child) = command.stdin(Stdio::null()).stdout(Stdio::piped()).spawn() else {
            return Vec::new();
        };
        let mut stdout = child.stdout.take().expect("piped");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(protocol::read::<Reply>(&mut stdout));
        });
        let found = match rx.recv_timeout(SCAN) {
            Ok(Ok(Reply::Scanned(found))) => found,
            _ => {
                let what = file.map_or("the built-in plugins".into(), |f| f.display().to_string());
                eprintln!("mdaw: {what} could not be scanned; its plugins are left out");
                Vec::new()
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        found
    }
}

impl Backend for IsolatedBackend {
    fn format(&self) -> PluginFormat {
        PluginFormat::Clap
    }

    fn scan(&mut self) -> Vec<PluginInfo> {
        let mut found: Vec<(PluginInfo, Option<PathBuf>)> = Vec::new();
        if self.builtin {
            found.extend(self.scan_one(None).into_iter().map(|p| (p, None)));
        }
        let files = self.files.clone().unwrap_or_else(plugin_files);
        // Each file in a helper of its own, several at once.
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = files
                .iter()
                .map(|file| scope.spawn(|| (file.clone(), self.scan_one(Some(file)))))
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (file, infos) in results {
            for info in infos {
                if !found.iter().any(|(p, _)| p.id == info.id) {
                    found.push((info, Some(file.clone())));
                }
            }
        }
        self.found = found;
        self.found.iter().map(|(p, _)| p.clone()).collect()
    }

    fn create(&mut self, id: &str, settings: Settings) -> Result<Box<dyn Instance>, PluginError> {
        let (_, file) = self
            .found
            .iter()
            .find(|(p, _)| p.id == id)
            .ok_or_else(|| error(format!("CLAP plugin {id} is not installed")))?;
        let instance =
            IsolatedInstance::start(&self.helper, file.as_deref(), id, settings, self.waker.clone())?;
        Ok(Box::new(instance))
    }

    fn create_later(&mut self, id: &str, settings: Settings) -> Option<Result<Creation, PluginError>> {
        let Some((_, file)) = self.found.iter().find(|(p, _)| p.id == id) else {
            return Some(Err(error(format!("CLAP plugin {id} is not installed"))));
        };
        let (helper, file, id, waker) = (
            self.helper.clone(),
            file.clone(),
            id.to_string(),
            self.waker.clone(),
        );
        Some(Ok(Box::new(move || {
            let instance = IsolatedInstance::start(&helper, file.as_deref(), &id, settings, waker)?;
            Ok(Box::new(instance) as Box<dyn Instance + Send>)
        })))
    }

    fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker);
    }
}

/// Names the files of shared blocks.
static BLOCKS: AtomicU64 = AtomicU64::new(0);

fn block_path() -> PathBuf {
    let dir = Path::new("/dev/shm");
    let dir = if dir.is_dir() {
        dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let n = BLOCKS.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("mdaw-{}-{n}", std::process::id()))
}

struct Helper {
    process: std::process::Child,
    stdin: ChildStdin,
    replies: Receiver<std::io::Result<Reply>>,
}

/// What the helper last said about itself, unasked.
#[derive(Default)]
struct Status {
    changed: AtomicBool,
    editor_open: AtomicBool,
    param_count: AtomicU32,
}

/// An instance in a helper process.
pub struct IsolatedInstance {
    info: PluginInfo,
    helper: Option<Helper>,
    /// Set once the helper is gone: the processor stops waiting for it.
    dead: Arc<AtomicBool>,
    /// Since when the helper's audio has not come back, on the engine's clock; 0 while it
    /// comes.
    stuck_since: Arc<AtomicI64>,
    processor: Option<Box<IsolatedProcessor>>,
    has_editor: bool,
    status: Arc<Status>,
    /// A parameter was set from here since the helper last said it was unchanged.
    changed: bool,
}

impl IsolatedInstance {
    fn start(
        helper: &Path,
        file: Option<&Path>,
        id: &str,
        settings: Settings,
        waker: Option<Waker>,
    ) -> Result<Self, PluginError> {
        let path = block_path();
        let shared = Arc::new(Shared::create(&path).map_err(|e| error(format!("a shared block: {e}")))?);
        let result = Self::spawn(helper, file, id, settings, &path, shared, waker);
        // Both sides have it mapped, or it is of no use: the name can go.
        let _ = std::fs::remove_file(&path);
        result
    }

    fn spawn(
        helper: &Path,
        file: Option<&Path>,
        id: &str,
        settings: Settings,
        path: &Path,
        shared: Arc<Shared>,
        waker: Option<Waker>,
    ) -> Result<Self, PluginError> {
        let mut process = Command::new(helper)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| error(format!("starting {}: {e}", helper.display())))?;
        let stdin = process.stdin.take().expect("piped");
        let stdout = process.stdout.take().expect("piped");
        let (tx, replies) = channel();
        let dead = Arc::new(AtomicBool::new(false));
        let stuck_since = Arc::new(AtomicI64::new(0));
        let status = Arc::new(Status::default());
        {
            // The helper's stdout ends the moment it does: whoever waits for its audio stops
            // waiting at once. What it says about itself unasked is kept for the main thread,
            // which is woken for it.
            let (dead, shared, status) = (dead.clone(), shared.clone(), status.clone());
            std::thread::spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let wake = || {
                    if let Some(waker) = &waker {
                        waker();
                    }
                };
                loop {
                    let reply = protocol::read::<Reply>(&mut stdout);
                    match &reply {
                        Ok(Reply::Status {
                            changed,
                            editor_open,
                            param_count,
                        }) => {
                            status.changed.store(*changed, Ordering::Release);
                            status.editor_open.store(*editor_open, Ordering::Release);
                            status.param_count.store(*param_count, Ordering::Release);
                            wake();
                            continue;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            dead.store(true, Ordering::Release);
                            let done = &shared.block().done;
                            done.fetch_add(1, Ordering::AcqRel);
                            shm::wake(done);
                            wake();
                        }
                    }
                    let end = reply.is_err();
                    if tx.send(reply).is_err() || end {
                        return;
                    }
                }
            });
        }
        let mut instance = Self {
            info: PluginInfo {
                format: PluginFormat::Clap,
                id: id.to_string(),
                name: id.to_string(),
                vendor: String::new(),
                instrument: true,
                location: String::new(),
            },
            helper: Some(Helper {
                process,
                stdin,
                replies,
            }),
            dead: dead.clone(),
            stuck_since: stuck_since.clone(),
            processor: Some(Box::new(IsolatedProcessor::new(
                shared,
                dead,
                stuck_since,
                settings,
            ))),
            has_editor: false,
            status,
            changed: false,
        };
        let request = Request::Create {
            file: file.map(|f| f.display().to_string()),
            id: id.to_string(),
            settings,
            shm: path.display().to_string(),
        };
        match instance.ask(request, LONG_ANSWER) {
            Some(Reply::Created {
                info,
                has_editor,
                param_count,
            }) => {
                instance.info = info;
                instance.has_editor = has_editor;
                instance.status.param_count.store(param_count, Ordering::Release);
                Ok(instance)
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("the helper for {id} did not start"))),
        }
    }

    /// Asks the helper and waits for the answer. `None`, and the helper is let go, if it does
    /// not answer in time or is gone.
    fn ask(&mut self, request: Request, timeout: Duration) -> Option<Reply> {
        let helper = self.helper.as_mut()?;
        if protocol::write(&mut helper.stdin, &request).is_err() {
            self.lose();
            return None;
        }
        match helper.replies.recv_timeout(timeout) {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                self.lose();
                None
            }
        }
    }

    /// The helper crashed, hangs or is gone: it is ended, and the instance stays silent.
    fn lose(&mut self) {
        self.dead.store(true, Ordering::Release);
        self.status.editor_open.store(false, Ordering::Release);
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.process.kill();
            let _ = helper.process.wait();
        }
    }
}

impl Instance for IsolatedInstance {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn take_processor(&mut self) -> Option<Box<dyn ExternalDevice>> {
        Some(self.processor.take()?)
    }

    fn busy(&self) -> bool {
        false
    }

    fn save_state(&mut self) -> Result<Vec<u8>, PluginError> {
        match self.ask(Request::SaveState, LONG_ANSWER) {
            Some(Reply::State(bytes)) => {
                self.changed = false;
                Ok(bytes)
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        match self.ask(Request::LoadState(bytes.to_vec()), LONG_ANSWER) {
            Some(Reply::Done) => {
                self.changed = false;
                Ok(())
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn params(&mut self, limit: usize) -> Vec<ParamInfo> {
        let request = Request::Params {
            limit: limit.min(u32::MAX as usize) as u32,
        };
        match self.ask(request, ANSWER) {
            Some(Reply::Params(params, count)) => {
                self.status.param_count.store(count, Ordering::Release);
                params
            }
            _ => Vec::new(),
        }
    }

    fn param_count(&self) -> usize {
        self.status.param_count.load(Ordering::Acquire) as usize
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if let Some(Reply::Done) = self.ask(Request::SetParam { id, value }, ANSWER) {
            self.changed = true;
        }
    }

    fn changed(&self) -> bool {
        self.changed || self.status.changed.load(Ordering::Acquire)
    }

    fn has_editor(&self) -> bool {
        self.has_editor
    }

    fn open_editor(&mut self, title: &str) -> Result<(), PluginError> {
        let request = Request::OpenEditor {
            title: title.to_string(),
        };
        match self.ask(request, LONG_ANSWER) {
            Some(Reply::Done) => {
                self.status.editor_open.store(true, Ordering::Release);
                Ok(())
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn close_editor(&mut self) {
        if self.editor_open() {
            self.ask(Request::CloseEditor, ANSWER);
        }
        self.status.editor_open.store(false, Ordering::Release);
    }

    fn editor_open(&self) -> bool {
        self.status.editor_open.load(Ordering::Acquire)
    }

    fn idle(&mut self) {
        let exited = match self.helper.as_mut() {
            Some(helper) => !matches!(helper.process.try_wait(), Ok(None)),
            None => return,
        };
        if exited {
            self.lose();
            return;
        }
        // Its window may still answer while its audio thread hangs in the plugin.
        let stuck = self.stuck_since.load(Ordering::Acquire);
        if stuck != 0 && mdaw_engine::clock::now_micros() - stuck > HUNG_MICROS {
            eprintln!("mdaw: {} stopped making sound and is ended", self.info.name);
            self.lose();
        }
    }

    fn crashed(&self) -> bool {
        self.helper.is_none() || self.dead.load(Ordering::Acquire)
    }

    fn needs_main_loop(&self) -> bool {
        // Its window and timers run in the helper, which says when it has something.
        false
    }
}

impl Drop for IsolatedInstance {
    fn drop(&mut self) {
        self.dead.store(true, Ordering::Release);
        let Some(mut helper) = self.helper.take() else {
            return;
        };
        let _ = protocol::write(&mut helper.stdin, &Request::Quit);
        // Waiting for it to finish is no business of the thread that let it go.
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_millis(500);
            while Instant::now() < until {
                if !matches!(helper.process.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = helper.process.kill();
            let _ = helper.process.wait();
        });
    }
}

/// The DAW's audio thread's end of an instance in a helper.
pub struct IsolatedProcessor {
    shared: Arc<Shared>,
    dead: Arc<AtomicBool>,
    stuck_since: Arc<AtomicI64>,
    pending: [Event; EVENTS],
    pending_len: usize,
    /// Keys of the notes that sound, for `release_where`.
    sounding: [Option<u64>; 128],
    sample_rate: u32,
    offline: bool,
}

impl IsolatedProcessor {
    fn new(
        shared: Arc<Shared>,
        dead: Arc<AtomicBool>,
        stuck_since: Arc<AtomicI64>,
        settings: Settings,
    ) -> Self {
        Self {
            shared,
            dead,
            stuck_since,
            pending: [Event::default(); EVENTS],
            pending_len: 0,
            sounding: [None; 128],
            sample_rate: settings.sample_rate.max(1),
            offline: settings.offline,
        }
    }

    /// Queues an event for the next block. Note starts leave room for note ends: a note that
    /// cannot start is a wrong note, one that cannot end is stuck.
    fn push(&mut self, kind: u32, offset: u32, key: u64, pitch: u8, vel: u8) -> bool {
        let room = if kind == NOTE_ON {
            EVENTS - EVENTS / 4
        } else {
            EVENTS
        };
        if self.pending_len >= room {
            return false;
        }
        self.pending[self.pending_len] = Event {
            kind,
            pitch: pitch as u32,
            vel: vel as u32,
            offset,
            key,
        };
        self.pending_len += 1;
        true
    }
}

impl ExternalDevice for IsolatedProcessor {
    fn note_on(&mut self, offset: u32, key: u64, pitch: u8, vel: u8) {
        // Only a note that can be ended later is started.
        let Some(slot) = self.sounding.iter().position(|s| s.is_none()) else {
            return;
        };
        if self.push(NOTE_ON, offset, key, pitch, vel) {
            self.sounding[slot] = Some(key);
        }
    }

    fn note_off(&mut self, offset: u32, key: u64) {
        if let Some(slot) = self.sounding.iter().position(|s| *s == Some(key))
            && self.push(NOTE_OFF, offset, key, 0, 0)
        {
            self.sounding[slot] = None;
        }
    }

    fn release_where(&mut self, filter: &dyn Fn(u64) -> bool) {
        for i in 0..self.sounding.len() {
            if let Some(key) = self.sounding[i].filter(|k| filter(*k))
                && self.push(NOTE_OFF, 0, key, 0, 0)
            {
                self.sounding[i] = None;
            }
        }
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32], time: &BlockTime) {
        if self.dead.load(Ordering::Acquire) {
            return;
        }
        let block = self.shared.block();
        let asked = block.request.load(Ordering::Acquire);
        if block.done.load(Ordering::Acquire) != asked {
            // Still busy with a block it was late for: this one goes without it.
            if self.stuck_since.load(Ordering::Relaxed) == 0 {
                self.stuck_since
                    .store(mdaw_engine::clock::now_micros().max(1), Ordering::Release);
            }
            return;
        }
        self.stuck_since.store(0, Ordering::Release);
        let frames = left.len().min(MAX_BLOCK);
        block.events[..self.pending_len].copy_from_slice(&self.pending[..self.pending_len]);
        block.events_len = self.pending_len as u32;
        self.pending_len = 0;
        block.frames = frames as u32;
        block.left[..frames].copy_from_slice(&left[..frames]);
        block.right[..frames].copy_from_slice(&right[..frames]);
        block.set_time(time);
        let seq = asked.wrapping_add(1);
        block.request.store(seq, Ordering::Release);
        shm::wake(&block.request);
        let started = Instant::now();
        let own = if self.offline {
            OFFLINE_WAIT
        } else {
            SLACK + Duration::from_micros(frames as u64 * 1_000_000 / self.sample_rate as u64)
        };
        // The callback's deadline is shared by every plugin in it: one that is late goes
        // without its sound, so that the whole output is never late.
        let until = match time.deadline {
            Some(deadline) => deadline.min(started + own),
            None => started + own,
        };
        loop {
            let done = block.done.load(Ordering::Acquire);
            if done == seq {
                break;
            }
            let Some(left_over) = until.checked_duration_since(Instant::now()) else {
                // Late: the block goes without it.
                return;
            };
            if self.dead.load(Ordering::Acquire) {
                return;
            }
            shm::wait(&block.done, done, left_over);
        }
        // A helper that died as it was writing leaves no half-written block behind.
        if self.dead.load(Ordering::Acquire) {
            return;
        }
        left[..frames].copy_from_slice(&block.left[..frames]);
        right[..frames].copy_from_slice(&block.right[..frames]);
    }
}
