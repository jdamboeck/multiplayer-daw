//! The DAW's side: plugins found and run by helper processes.

use super::protocol::{self, Reply, Request};
use super::shm::{self, EVENTS, Event, NOTE_OFF, NOTE_ON, Shared};
use crate::clap::plugin_files;
use crate::{Backend, Instance, ParamInfo, PluginError, PluginFormat, PluginInfo, Settings, error};
use mdaw_dsp::{BlockTime, ExternalDevice, MAX_BLOCK};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// How long a helper may take to answer the main thread before it counts as hung.
const ANSWER: Duration = Duration::from_secs(5);
/// How long scanning one file may take.
const SCAN: Duration = Duration::from_secs(20);
/// How often an instance asks its helper how it is.
const STATUS_EVERY: Duration = Duration::from_millis(50);
/// What a helper gets for a block on top of the block's own length, for waking up.
const SLACK: Duration = Duration::from_millis(2);
/// How long an offline render waits for a block.
const OFFLINE_WAIT: Duration = Duration::from_secs(10);

/// CLAP plugins, each run in a helper process of its own: a plugin that crashes takes only
/// its helper down.
pub struct IsolatedBackend {
    helper: PathBuf,
    /// The plugin files to look at: `None` for the usual places.
    files: Option<Vec<PathBuf>>,
    builtin: bool,
    /// What was found, with the file it is in (`None`: built in).
    found: Vec<(PluginInfo, Option<PathBuf>)>,
}

impl IsolatedBackend {
    /// The plugins installed in the usual places, and the test synth if `builtin`.
    pub fn standard(helper: PathBuf, builtin: bool) -> Self {
        Self {
            helper,
            files: None,
            builtin,
            found: Vec::new(),
        }
    }

    pub fn test_synth_only(helper: PathBuf) -> Self {
        Self {
            helper,
            files: Some(Vec::new()),
            builtin: true,
            found: Vec::new(),
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
        let instance = IsolatedInstance::start(&self.helper, file.as_deref(), id, settings)?;
        Ok(Box::new(instance))
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

/// An instance in a helper process.
pub struct IsolatedInstance {
    info: PluginInfo,
    helper: Option<Helper>,
    /// Set once the helper is gone: the processor stops waiting for it.
    dead: Arc<AtomicBool>,
    processor: Option<Box<IsolatedProcessor>>,
    has_editor: bool,
    param_count: usize,
    changed: bool,
    editor_open: bool,
    asked: Instant,
}

impl IsolatedInstance {
    fn start(helper: &Path, file: Option<&Path>, id: &str, settings: Settings) -> Result<Self, PluginError> {
        let path = block_path();
        let shared = Arc::new(Shared::create(&path).map_err(|e| error(format!("a shared block: {e}")))?);
        let result = Self::spawn(helper, file, id, settings, &path, shared);
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
        {
            // The helper's stdout ends the moment it does: whoever waits for its audio stops
            // waiting at once.
            let (dead, shared) = (dead.clone(), shared.clone());
            std::thread::spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    let reply = protocol::read::<Reply>(&mut stdout);
                    if reply.is_err() {
                        dead.store(true, Ordering::Release);
                        let done = &shared.block().done;
                        done.fetch_add(1, Ordering::AcqRel);
                        shm::wake(done);
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
            processor: Some(Box::new(IsolatedProcessor::new(shared, dead, settings))),
            has_editor: false,
            param_count: 0,
            changed: false,
            editor_open: false,
            asked: Instant::now(),
        };
        let request = Request::Create {
            file: file.map(|f| f.display().to_string()),
            id: id.to_string(),
            settings,
            shm: path.display().to_string(),
        };
        match instance.ask(request) {
            Some(Reply::Created {
                info,
                has_editor,
                param_count,
            }) => {
                instance.info = info;
                instance.has_editor = has_editor;
                instance.param_count = param_count as usize;
                Ok(instance)
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("the helper for {id} did not start"))),
        }
    }

    /// Asks the helper and waits for the answer. `None`, and the helper is let go, if it does
    /// not answer in time or is gone.
    fn ask(&mut self, request: Request) -> Option<Reply> {
        let helper = self.helper.as_mut()?;
        if protocol::write(&mut helper.stdin, &request).is_err() {
            self.lose();
            return None;
        }
        match helper.replies.recv_timeout(ANSWER) {
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
        self.editor_open = false;
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
        match self.ask(Request::SaveState) {
            Some(Reply::State(bytes)) => {
                self.changed = false;
                Ok(bytes)
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        match self.ask(Request::LoadState(bytes.to_vec())) {
            Some(Reply::Done) => {
                self.changed = false;
                Ok(())
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn params(&mut self, limit: usize) -> Vec<ParamInfo> {
        match self.ask(Request::Params {
            limit: limit.min(u32::MAX as usize) as u32,
        }) {
            Some(Reply::Params(params, count)) => {
                self.param_count = count as usize;
                params
            }
            _ => Vec::new(),
        }
    }

    fn param_count(&self) -> usize {
        self.param_count
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if let Some(Reply::Done) = self.ask(Request::SetParam { id, value }) {
            self.changed = true;
        }
    }

    fn changed(&self) -> bool {
        self.changed
    }

    fn has_editor(&self) -> bool {
        self.has_editor
    }

    fn open_editor(&mut self, title: &str) -> Result<(), PluginError> {
        match self.ask(Request::OpenEditor {
            title: title.to_string(),
        }) {
            Some(Reply::Done) => {
                self.editor_open = true;
                Ok(())
            }
            Some(Reply::Error(e)) => Err(error(e)),
            _ => Err(error(format!("{} is gone", self.info.name))),
        }
    }

    fn close_editor(&mut self) {
        if self.editor_open {
            self.ask(Request::CloseEditor);
        }
        self.editor_open = false;
    }

    fn editor_open(&self) -> bool {
        self.editor_open
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
        if self.asked.elapsed() >= STATUS_EVERY {
            self.asked = Instant::now();
            if let Some(Reply::Status {
                changed,
                editor_open,
                param_count,
            }) = self.ask(Request::Status)
            {
                self.changed |= changed;
                self.editor_open = editor_open;
                self.param_count = param_count as usize;
            }
        }
    }

    fn crashed(&self) -> bool {
        self.helper.is_none() || self.dead.load(Ordering::Acquire)
    }
}

impl Drop for IsolatedInstance {
    fn drop(&mut self) {
        self.dead.store(true, Ordering::Release);
        let Some(mut helper) = self.helper.take() else {
            return;
        };
        let _ = protocol::write(&mut helper.stdin, &Request::Quit);
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            if !matches!(helper.process.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = helper.process.kill();
        let _ = helper.process.wait();
    }
}

/// The DAW's audio thread's end of an instance in a helper.
pub struct IsolatedProcessor {
    shared: Arc<Shared>,
    dead: Arc<AtomicBool>,
    pending: [Event; EVENTS],
    pending_len: usize,
    /// Keys of the notes that sound, for `release_where`.
    sounding: [Option<u64>; 128],
    sample_rate: u32,
    offline: bool,
}

impl IsolatedProcessor {
    fn new(shared: Arc<Shared>, dead: Arc<AtomicBool>, settings: Settings) -> Self {
        Self {
            shared,
            dead,
            pending: [Event::default(); EVENTS],
            pending_len: 0,
            sounding: [None; 128],
            sample_rate: settings.sample_rate.max(1),
            offline: settings.offline,
        }
    }

    fn push(&mut self, kind: u32, key: u64, pitch: u8, vel: u8) {
        if self.pending_len < EVENTS {
            self.pending[self.pending_len] = Event {
                kind,
                pitch: pitch as u32,
                vel: vel as u32,
                _pad: 0,
                key,
            };
            self.pending_len += 1;
        }
    }
}

impl ExternalDevice for IsolatedProcessor {
    fn note_on(&mut self, key: u64, pitch: u8, vel: u8) {
        self.push(NOTE_ON, key, pitch, vel);
        if let Some(slot) = self.sounding.iter_mut().find(|s| s.is_none()) {
            *slot = Some(key);
        }
    }

    fn note_off(&mut self, key: u64) {
        self.push(NOTE_OFF, key, 0, 0);
        if let Some(slot) = self.sounding.iter_mut().find(|s| **s == Some(key)) {
            *slot = None;
        }
    }

    fn release_where(&mut self, filter: &dyn Fn(u64) -> bool) {
        for i in 0..self.sounding.len() {
            if let Some(key) = self.sounding[i].filter(|k| filter(*k)) {
                self.sounding[i] = None;
                self.push(NOTE_OFF, key, 0, 0);
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
            return;
        }
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
        let wait = if self.offline {
            OFFLINE_WAIT
        } else {
            SLACK + Duration::from_micros(frames as u64 * 1_000_000 / self.sample_rate as u64)
        };
        let started = Instant::now();
        loop {
            let done = block.done.load(Ordering::Acquire);
            if done == seq {
                break;
            }
            let Some(left_over) = wait.checked_sub(started.elapsed()) else {
                // Late: the block goes without it.
                return;
            };
            if self.dead.load(Ordering::Acquire) {
                return;
            }
            shm::wait(&block.done, done, left_over);
        }
        left[..frames].copy_from_slice(&block.left[..frames]);
        right[..frames].copy_from_slice(&block.right[..frames]);
    }
}
