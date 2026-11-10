//! Host-side handlers and thin wrappers around a clack plugin instance.
//!
//! Three handler objects, matching CLAP's threading model:
//! `Shared` (any thread), `MainThread` (main thread only), and `()` for the audio thread.

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortRescanFlags, HostAudioPorts, HostAudioPortsImpl,
    PluginAudioPorts,
};
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl, PluginGui};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{
    HostNotePorts, HostNotePortsImpl, NoteDialects, NotePortRescanFlags, PluginNotePorts,
};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
#[cfg(unix)]
use clack_extensions::posix_fd::{FdFlags, HostPosixFd, HostPosixFdImpl, PluginPosixFd};
use clack_extensions::render::{PluginRender, RenderMode};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_extensions::thread_check::{HostThreadCheck, HostThreadCheckImpl};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

pub struct SpikeHost;

impl HostHandlers for SpikeHost {
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostThreadCheck>()
            .register::<HostGui>()
            .register::<HostParams>()
            .register::<HostState>()
            .register::<HostTimer>()
            .register::<HostAudioPorts>()
            .register::<HostNotePorts>();
        #[cfg(unix)]
        builder.register::<HostPosixFd>();
    }
}

// ---------------------------------------------------------------- shared

pub struct Shared {
    main_thread: ThreadId,
    pub callback_requested: AtomicBool,
    pub restart_requested: AtomicBool,
    pub flush_requested: AtomicBool,
    pub gui_closed: AtomicBool,
    /// Packed `GuiSize` the plugin asked for; 0 means no pending request.
    pub resize_requested: AtomicU64,
    /// Set while the main thread plays the audio-thread role for an offline render.
    pub offline_processing: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            main_thread: std::thread::current().id(),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            flush_requested: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            resize_requested: AtomicU64::new(0),
            offline_processing: AtomicBool::new(false),
        }
    }
}

impl SharedHandler<'_> for Shared {
    fn request_restart(&self) {
        self.restart_requested.store(true, Relaxed);
    }
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.callback_requested.store(true, Relaxed);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, severity: LogSeverity, message: &str) {
        eprintln!("    [plugin {severity}] {message}");
    }
}

impl HostThreadCheckImpl for Shared {
    fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread
    }
    fn is_audio_thread(&self) -> bool {
        !self.is_main_thread() || self.offline_processing.load(Relaxed)
    }
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        self.resize_requested.store(new_size.pack_to_u64(), Relaxed);
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        self.gui_closed.store(true, Relaxed);
    }
}

impl HostParamsImplShared for Shared {
    fn request_flush(&self) {
        self.flush_requested.store(true, Relaxed);
    }
}

// ---------------------------------------------------------------- main thread

/// Plugin-side extensions, looked up once after `init()`.
#[derive(Clone, Copy, Default)]
pub struct Exts {
    pub audio_ports: Option<PluginAudioPorts>,
    pub note_ports: Option<PluginNotePorts>,
    pub params: Option<PluginParams>,
    pub state: Option<PluginState>,
    pub gui: Option<PluginGui>,
    pub timer: Option<PluginTimer>,
    pub render: Option<PluginRender>,
    #[cfg(unix)]
    pub posix_fd: Option<PluginPosixFd>,
}

impl Exts {
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        let mut add = |present: bool, name| {
            if present {
                v.push(name)
            }
        };
        add(self.audio_ports.is_some(), "audio-ports");
        add(self.note_ports.is_some(), "note-ports");
        add(self.params.is_some(), "params");
        add(self.state.is_some(), "state");
        add(self.gui.is_some(), "gui");
        add(self.timer.is_some(), "timer-support");
        add(self.render.is_some(), "render");
        #[cfg(unix)]
        add(self.posix_fd.is_some(), "posix-fd-support");
        v
    }
}

struct Timer {
    id: TimerId,
    period: Duration,
    next: Instant,
}

pub struct MainThread<'a> {
    #[allow(dead_code)]
    shared: &'a Shared,
    exts: OnceCell<Exts>,
    timers: RefCell<Vec<Timer>>,
    next_timer_id: Cell<u32>,
    #[cfg(unix)]
    fds: RefCell<Vec<(std::os::fd::RawFd, FdFlags)>>,
    pub state_dirty: Cell<bool>,
    pub timers_registered: Cell<u32>,
    pub fds_registered: Cell<u32>,
}

impl<'a> MainThread<'a> {
    fn new(shared: &'a Shared) -> Self {
        Self {
            shared,
            exts: OnceCell::new(),
            timers: RefCell::new(Vec::new()),
            next_timer_id: Cell::new(1),
            #[cfg(unix)]
            fds: RefCell::new(Vec::new()),
            state_dirty: Cell::new(false),
            timers_registered: Cell::new(0),
            fds_registered: Cell::new(0),
        }
    }

    fn due_timers(&self, now: Instant) -> Vec<TimerId> {
        let mut due = Vec::new();
        for t in self.timers.borrow_mut().iter_mut() {
            if t.next <= now {
                due.push(t.id);
                t.next = now + t.period;
            }
        }
        due
    }
}

impl<'a> MainThreadHandler<'a> for MainThread<'a> {
    fn initialized(&self, instance: InitializedPluginHandle<'a>) {
        let _ = self.exts.set(Exts {
            audio_ports: instance.get_extension(),
            note_ports: instance.get_extension(),
            params: instance.get_extension(),
            state: instance.get_extension(),
            gui: instance.get_extension(),
            timer: instance.get_extension(),
            render: instance.get_extension(),
            #[cfg(unix)]
            posix_fd: instance.get_extension(),
        });
    }
}

impl HostParamsImplMainThread for MainThread<'_> {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostStateImpl for MainThread<'_> {
    fn mark_dirty(&self) {
        self.state_dirty.set(true);
    }
}

impl HostTimerImpl for MainThread<'_> {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        let id = TimerId(self.next_timer_id.get());
        self.next_timer_id.set(id.0 + 1);
        let period = Duration::from_millis(period_ms.max(1) as u64);
        self.timers.borrow_mut().push(Timer {
            id,
            period,
            next: Instant::now() + period,
        });
        self.timers_registered.set(self.timers_registered.get() + 1);
        Ok(id)
    }
    fn unregister_timer(&self, timer_id: TimerId) -> Result<(), HostError> {
        self.timers.borrow_mut().retain(|t| t.id != timer_id);
        Ok(())
    }
}

impl HostAudioPortsImpl for MainThread<'_> {
    fn is_rescan_flag_supported(&self, _flag: AudioPortRescanFlags) -> bool {
        false
    }
    fn rescan(&self, _flags: AudioPortRescanFlags) {}
}

impl HostNotePortsImpl for MainThread<'_> {
    fn supported_dialects(&self) -> NoteDialects {
        NoteDialects::CLAP
    }
    fn rescan(&self, _flags: NotePortRescanFlags) {}
}

#[cfg(unix)]
impl HostPosixFdImpl for MainThread<'_> {
    fn register_fd(&self, fd: std::os::fd::RawFd, flags: FdFlags) -> Result<(), HostError> {
        self.fds.borrow_mut().push((fd, flags));
        self.fds_registered.set(self.fds_registered.get() + 1);
        Ok(())
    }
    fn modify_fd(&self, fd: std::os::fd::RawFd, flags: FdFlags) -> Result<(), HostError> {
        for entry in self.fds.borrow_mut().iter_mut().filter(|e| e.0 == fd) {
            entry.1 = flags;
        }
        Ok(())
    }
    fn unregister_fd(&self, fd: std::os::fd::RawFd) -> Result<(), HostError> {
        self.fds.borrow_mut().retain(|e| e.0 != fd);
        Ok(())
    }
}

// ---------------------------------------------------------------- plugin wrapper

#[derive(Clone, Debug)]
pub struct Port {
    pub name: String,
    pub channels: u32,
    pub main: bool,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub id: ClapId,
    /// Opaque plugin pointer for this parameter. Hosts are expected to hand it back with
    /// every parameter event; some plugins crash on a null cookie (Odin2 2.4.1 does).
    pub cookie: Cookie,
    pub name: String,
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub flags: ParamInfoFlags,
    pub value: Option<f64>,
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub struct Plugin {
    pub instance: PluginInstance<SpikeHost>,
    pub exts: Exts,
    pub audio_in: Vec<Port>,
    pub audio_out: Vec<Port>,
    pub note_in: u32,
}

impl Plugin {
    /// Must be called on the thread that will act as this plugin's main thread.
    pub fn new(entry: &PluginEntry, plugin_id: &str) -> Result<Self, String> {
        let host = HostInfo::new("mdaw-spike", "mdaw", "https://example.invalid", "0.0.0")
            .map_err(|e| e.to_string())?;
        let id = CString::new(plugin_id).map_err(|e| e.to_string())?;
        let instance = PluginInstance::<SpikeHost>::new(
            |_| Shared::new(),
            |shared| MainThread::new(shared),
            entry,
            &id,
            &host,
        )
        .map_err(|e| format!("instantiate: {e}"))?;
        let exts = instance
            .access_handler(|m| m.exts.get().copied())
            .unwrap_or_default();
        let mut plugin = Self {
            instance,
            exts,
            audio_in: Vec::new(),
            audio_out: Vec::new(),
            note_in: 0,
        };
        plugin.audio_in = plugin.audio_ports(true);
        plugin.audio_out = plugin.audio_ports(false);
        if let Some(ext) = exts.note_ports {
            plugin.note_in = ext.count(&plugin.instance.plugin_handle(), true);
        }
        Ok(plugin)
    }

    fn audio_ports(&mut self, is_input: bool) -> Vec<Port> {
        let Some(ext) = self.exts.audio_ports else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let mut buffer = AudioPortInfoBuffer::new();
        (0..ext.count(&handle, is_input))
            .filter_map(|i| {
                ext.get(&handle, i, is_input, &mut buffer).map(|info| Port {
                    name: text(info.name),
                    channels: info.channel_count,
                    main: info.flags.contains(AudioPortFlags::IS_MAIN),
                })
            })
            .collect()
    }

    pub fn params(&mut self) -> Vec<Param> {
        let Some(ext) = self.exts.params else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let mut buffer = ParamInfoBuffer::new();
        (0..ext.count(&handle))
            .filter_map(|i| {
                let info = ext.get_info(&handle, i, &mut buffer)?;
                Some(Param {
                    id: info.id,
                    cookie: info.cookie,
                    name: text(info.name),
                    module: text(info.module),
                    min: info.min_value,
                    max: info.max_value,
                    default: info.default_value,
                    flags: info.flags,
                    value: ext.get_value(&handle, info.id),
                })
            })
            .collect()
    }

    pub fn param_value(&mut self, id: ClapId) -> Option<f64> {
        self.exts.params?.get_value(&self.instance.plugin_handle(), id)
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>, String> {
        let ext = self.exts.state.ok_or("plugin has no state extension")?;
        let mut out = Vec::new();
        ext.save(&self.instance.plugin_handle(), &mut out)
            .map_err(|e| format!("save: {e}"))?;
        Ok(out)
    }

    pub fn load_state(&mut self, mut data: &[u8]) -> Result<(), String> {
        let ext = self.exts.state.ok_or("plugin has no state extension")?;
        ext.load(&self.instance.plugin_handle(), &mut data)
            .map_err(|e| format!("load: {e}"))
    }

    pub fn set_offline(&mut self, offline: bool) -> Option<bool> {
        let ext = self.exts.render?;
        let mode = if offline {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        };
        Some(ext.set(&self.instance.plugin_handle(), mode).is_ok())
    }

    pub fn shared<R>(&self, f: impl FnOnce(&Shared) -> R) -> R {
        self.instance.access_shared_handler(|s| f(s))
    }

    /// Runs everything the plugin expects from the host's main loop:
    /// requested callbacks, timers and fd readiness.
    pub fn pump(&mut self) {
        if self.shared(|s| s.callback_requested.swap(false, Relaxed)) {
            self.instance.call_on_main_thread_callback();
        }
        if let Some(ext) = self.exts.timer {
            let due = self.instance.access_handler(|m| m.due_timers(Instant::now()));
            for id in due {
                ext.on_timer(&self.instance.plugin_handle(), id);
            }
        }
        #[cfg(unix)]
        if let Some(ext) = self.exts.posix_fd {
            let fds = self.instance.access_handler(|m| m.fds.borrow().clone());
            if fds.is_empty() {
                return;
            }
            let mut poll: Vec<libc::pollfd> = fds
                .iter()
                .map(|&(fd, flags)| {
                    let mut events = 0;
                    if flags.contains(FdFlags::READ) {
                        events |= libc::POLLIN;
                    }
                    if flags.contains(FdFlags::WRITE) {
                        events |= libc::POLLOUT;
                    }
                    libc::pollfd {
                        fd,
                        events,
                        revents: 0,
                    }
                })
                .collect();
            // SAFETY: `poll` points at `poll.len()` valid pollfd structs; zero timeout.
            let ready = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, 0) };
            if ready <= 0 {
                return;
            }
            for p in poll.iter().filter(|p| p.revents != 0) {
                let mut flags = FdFlags::empty();
                if p.revents & libc::POLLIN != 0 {
                    flags |= FdFlags::READ;
                }
                if p.revents & libc::POLLOUT != 0 {
                    flags |= FdFlags::WRITE;
                }
                if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    flags |= FdFlags::ERROR;
                }
                ext.on_fd(&self.instance.plugin_handle(), p.fd, flags);
            }
        }
    }

    pub fn pump_for(&mut self, duration: Duration) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn activate(&mut self, sample_rate: f64, max_frames: u32) -> Result<Processor, String> {
        let stopped = self
            .instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate,
                    min_frames_count: 1,
                    max_frames_count: max_frames,
                },
            )
            .map_err(|e| format!("activate: {e}"))?;
        let buffers = |ports: &[Port]| -> Vec<Vec<Vec<f32>>> {
            ports
                .iter()
                .map(|p| vec![vec![0.0; max_frames as usize]; p.channels as usize])
                .collect()
        };
        let channels = |ports: &[Port]| ports.iter().map(|p| p.channels as usize).sum::<usize>();
        Ok(Processor {
            processor: stopped.into(),
            in_ports: AudioPorts::with_capacity(channels(&self.audio_in), self.audio_in.len()),
            out_ports: AudioPorts::with_capacity(channels(&self.audio_out), self.audio_out.len()),
            ins: buffers(&self.audio_in),
            outs: buffers(&self.audio_out),
            events_in: EventBuffer::with_capacity(64),
            events_out: EventBuffer::with_capacity(64),
            steady_time: 0,
            max_frames: max_frames as usize,
            events_from_plugin: 0,
            main_in: self.audio_in.iter().position(|p| p.main).unwrap_or(0),
            main_out: self.audio_out.iter().position(|p| p.main).unwrap_or(0),
        })
    }

    pub fn deactivate(&mut self, processor: Processor) {
        self.instance.deactivate(processor.processor.into_stopped());
    }
}

// ---------------------------------------------------------------- audio side

#[derive(Clone, Copy, Debug)]
pub enum Ev {
    On { key: u8, velocity: f64 },
    Off { key: u8 },
    Param { id: ClapId, cookie: Cookie, value: f64 },
}

/// Owns the audio-thread half of a plugin plus the buffers it processes into.
pub struct Processor {
    processor: PluginAudioProcessor<SpikeHost>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    /// `[port][channel][frame]`
    pub ins: Vec<Vec<Vec<f32>>>,
    pub outs: Vec<Vec<Vec<f32>>>,
    events_in: EventBuffer,
    events_out: EventBuffer,
    steady_time: u64,
    pub max_frames: usize,
    pub events_from_plugin: u64,
    main_in: usize,
    main_out: usize,
}

impl Processor {
    /// Left and right of the main output port after `process` (mono is duplicated).
    pub fn main_out(&self, frames: usize) -> Option<(&[f32], &[f32])> {
        let port = self.outs.get(self.main_out)?;
        let left = port.first()?;
        let right = port.get(1).unwrap_or(left);
        Some((&left[..frames], &right[..frames]))
    }

    /// Fills every channel of the main input port from `signal(frame_index)`.
    pub fn fill_main_in(&mut self, frames: usize, mut signal: impl FnMut(usize) -> f32) {
        let Some(port) = self.ins.get_mut(self.main_in) else {
            return;
        };
        for i in 0..frames {
            let v = signal(i);
            for ch in port.iter_mut() {
                ch[i] = v;
            }
        }
    }

    /// Processes `frames` frames. `events` carry sample offsets inside this block.
    pub fn process(&mut self, frames: usize, events: &[(u32, Ev)]) -> Result<ProcessStatus, String> {
        assert!(frames <= self.max_frames);
        self.events_in.clear();
        self.events_out.clear();
        for &(time, ev) in events {
            match ev {
                Ev::On { key, velocity } => self.events_in.push(&NoteOnEvent::new(
                    time,
                    Pckn::new(0u16, 0u16, key as u16, key as u32),
                    velocity,
                )),
                Ev::Off { key } => self.events_in.push(&NoteOffEvent::new(
                    time,
                    Pckn::new(0u16, 0u16, key as u16, key as u32),
                    0.0,
                )),
                Ev::Param { id, cookie, value } => {
                    let event = ParamValueEvent::new(time, id, Pckn::match_all(), value);
                    // SAFETY: the cookie comes from this plugin's own `get_info` for `id`,
                    // and the spike never triggers a parameter rescan in between.
                    self.events_in.push(&unsafe { event.with_cookie(cookie) })
                }
            }
        }

        let inputs = self.in_ports.with_input_buffers(self.ins.iter_mut().map(|port| {
            AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    port.iter_mut()
                        .map(|ch| InputChannel::variable(&mut ch[..frames])),
                ),
            }
        }));
        let mut outputs = self
            .out_ports
            .with_output_buffers(self.outs.iter_mut().map(|port| AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(port.iter_mut().map(|ch| &mut ch[..frames])),
            }));

        let started = self
            .processor
            .ensure_processing_started()
            .map_err(|e| e.to_string())?;
        let status = started
            .process(
                &inputs,
                &mut outputs,
                &self.events_in.as_input(),
                &mut self.events_out.as_output(),
                Some(self.steady_time),
                None,
            )
            .map_err(|e| format!("process: {e}"))?;
        self.steady_time += frames as u64;
        self.events_from_plugin += self.events_out.len() as u64;
        Ok(status)
    }

    pub fn stop(&mut self) {
        self.processor.ensure_processing_stopped();
    }
}
