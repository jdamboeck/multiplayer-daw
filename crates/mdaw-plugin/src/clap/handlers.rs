//! What the host offers a CLAP plugin, in three parts after CLAP's threads: [`Shared`] for any
//! thread, [`MainThread`] for the thread that made the instance, and nothing for the audio
//! thread.

use clack_extensions::audio_ports::{
    AudioPortRescanFlags, HostAudioPorts, HostAudioPortsImpl, PluginAudioPorts,
};
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl, PluginGui};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::note_ports::{HostNotePorts, HostNotePortsImpl, NoteDialects, NotePortRescanFlags};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
    PluginParams,
};
#[cfg(unix)]
use clack_extensions::posix_fd::{FdFlags, HostPosixFd, HostPosixFdImpl, PluginPosixFd};
use clack_extensions::render::PluginRender;
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_extensions::thread_check::{HostThreadCheck, HostThreadCheckImpl};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_host::prelude::*;
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

pub struct MdawHost;

impl HostHandlers for MdawHost {
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

pub struct Shared {
    main_thread: ThreadId,
    pub callback_requested: AtomicBool,
    pub gui_closed: AtomicBool,
    /// Packed `GuiSize` the plugin asked for; 0 for none.
    pub resize_requested: AtomicU64,
    /// The state changed: the plugin said so, a parameter changed in its editor, or the host
    /// set one.
    pub changed: AtomicBool,
    /// The instance renders offline, its processing on the main thread.
    pub offline: AtomicBool,
    /// Its processing failed: it makes no more sound.
    pub failed: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            main_thread: std::thread::current().id(),
            callback_requested: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            resize_requested: AtomicU64::new(0),
            changed: AtomicBool::new(false),
            offline: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }
}

impl SharedHandler<'_> for Shared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.callback_requested.store(true, Relaxed);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if matches!(
            severity,
            LogSeverity::Error | LogSeverity::Fatal | LogSeverity::HostMisbehaving
        ) {
            eprintln!("mdaw: plugin says ({severity}): {message}");
        }
    }
}

impl HostThreadCheckImpl for Shared {
    fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread
    }
    fn is_audio_thread(&self) -> bool {
        !self.is_main_thread() || self.offline.load(Relaxed)
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
        // Parameters are flushed through `process`, which runs all the time.
    }
}

/// The plugin's extensions, looked up once after it is initialised.
#[derive(Clone, Copy, Default)]
pub struct Exts {
    pub audio_ports: Option<PluginAudioPorts>,
    pub params: Option<PluginParams>,
    pub state: Option<PluginState>,
    pub gui: Option<PluginGui>,
    pub timer: Option<PluginTimer>,
    pub render: Option<PluginRender>,
    #[cfg(unix)]
    pub posix_fd: Option<PluginPosixFd>,
}

struct Timer {
    id: TimerId,
    period: Duration,
    next: Instant,
}

pub struct MainThread<'a> {
    shared: &'a Shared,
    pub exts: OnceCell<Exts>,
    timers: RefCell<Vec<Timer>>,
    next_timer: Cell<u32>,
    #[cfg(unix)]
    pub fds: RefCell<Vec<(std::os::fd::RawFd, FdFlags)>>,
    /// The plugin changed its parameters: their descriptions are to be read again.
    pub params_changed: Cell<bool>,
}

impl<'a> MainThread<'a> {
    fn new(shared: &'a Shared) -> Self {
        Self {
            shared,
            exts: OnceCell::new(),
            timers: RefCell::new(Vec::new()),
            next_timer: Cell::new(1),
            #[cfg(unix)]
            fds: RefCell::new(Vec::new()),
            params_changed: Cell::new(false),
        }
    }

    pub fn due_timers(&self, now: Instant) -> Vec<TimerId> {
        let mut due = Vec::new();
        for timer in self.timers.borrow_mut().iter_mut() {
            if timer.next <= now {
                due.push(timer.id);
                timer.next = now + timer.period;
            }
        }
        due
    }
}

/// Makes the two handlers of a new instance.
pub fn handlers() -> (
    impl FnOnce(&()) -> Shared,
    impl for<'a> FnOnce(&'a Shared) -> MainThread<'a>,
) {
    (|_: &()| Shared::new(), |shared| MainThread::new(shared))
}

impl<'a> MainThreadHandler<'a> for MainThread<'a> {
    fn initialized(&self, instance: InitializedPluginHandle<'a>) {
        let _ = self.exts.set(Exts {
            audio_ports: instance.get_extension(),
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
    fn rescan(&self, flags: ParamRescanFlags) {
        if flags.intersects(ParamRescanFlags::ALL | ParamRescanFlags::INFO | ParamRescanFlags::TEXT) {
            self.params_changed.set(true);
        }
    }
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostStateImpl for MainThread<'_> {
    fn mark_dirty(&self) {
        self.shared.changed.store(true, Relaxed);
    }
}

impl HostTimerImpl for MainThread<'_> {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        let id = TimerId(self.next_timer.get());
        self.next_timer.set(id.0 + 1);
        let period = Duration::from_millis(period_ms.max(1) as u64);
        self.timers.borrow_mut().push(Timer {
            id,
            period,
            next: Instant::now() + period,
        });
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
