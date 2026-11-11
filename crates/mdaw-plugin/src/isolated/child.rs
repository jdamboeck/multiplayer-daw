//! The helper's side: one plugin, its main loop, and an audio thread that serves the DAW's
//! audio thread through the shared block.

use super::protocol::{self, Reply, Request};
use super::shm::{self, NOTE_OFF, NOTE_ON, Shared};
use crate::clap::ClapBackend;
use crate::{Backend, ExternalDevice, Instance, PluginFormat};
use mdaw_dsp::MAX_BLOCK;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How long the main loop sleeps between rounds: plugins' timers and windows run in them.
const MAIN_LOOP: Duration = Duration::from_millis(4);

/// The helper's `main`. `mdaw-plugin-host scan <file>|builtin` describes what a file holds;
/// `mdaw-plugin-host run` serves one instance.
pub fn main() -> ExitCode {
    // The protocol gets stdout to itself; what anybody else prints goes to stderr.
    // SAFETY: plain descriptor calls on the process's own standard streams.
    let mut out = unsafe {
        let fd = libc::dup(1);
        libc::dup2(2, 1);
        File::from_raw_fd(fd)
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scan") => {
            let backend = match args.get(1).map(String::as_str) {
                Some("builtin") | None => ClapBackend::files(Vec::new(), true),
                Some(file) => ClapBackend::files(vec![PathBuf::from(file)], false),
            };
            let mut backend = backend;
            let found = backend.scan();
            let _ = protocol::write(&mut out, &Reply::Scanned(found));
            ExitCode::SUCCESS
        }
        Some("run") => run(out),
        _ => {
            eprintln!("mdaw-plugin-host: runs one plugin for mdaw; not for use by hand");
            ExitCode::from(2)
        }
    }
}

struct Running {
    /// Keeps the plugin's file loaded.
    _backend: ClapBackend,
    instance: Box<dyn Instance>,
    shared: Arc<Shared>,
    audio: JoinHandle<()>,
}

fn run(mut out: File) -> ExitCode {
    let (requests, inbox) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(request) = protocol::read::<Request>(&mut stdin) {
            if requests.send(request).is_err() {
                return;
            }
        }
        // The DAW is gone. Quit, and should a plugin keep the main loop from getting there, end
        // regardless: a helper never outlives its DAW for long.
        let _ = requests.send(Request::Quit);
        std::thread::sleep(Duration::from_secs(2));
        // SAFETY: ending the process at once, without unwinding: nothing of it is needed any more.
        unsafe { libc::_exit(0) };
    });
    let mut running: Option<Running> = None;
    // What the DAW was last told about the instance.
    let mut told = (false, false, 0u32);
    loop {
        let request = match inbox.recv_timeout(MAIN_LOOP) {
            Ok(request) => Some(request),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Request::Quit),
        };
        if let Some(request) = request {
            if matches!(request, Request::Quit) {
                break;
            }
            let reply = handle(&mut running, request);
            if protocol::write(&mut out, &reply).is_err() {
                break;
            }
        }
        if let Some(running) = running.as_mut() {
            running.instance.idle();
            // Said unasked, so that the DAW never has to wait for it.
            let instance = &running.instance;
            let now = (
                instance.changed(),
                instance.editor_open(),
                instance.param_count() as u32,
            );
            if now != told {
                told = now;
                let status = Reply::Status {
                    changed: now.0,
                    editor_open: now.1,
                    param_count: now.2,
                };
                if protocol::write(&mut out, &status).is_err() {
                    break;
                }
            }
        }
    }
    if let Some(running) = running {
        let block = running.shared.block();
        block.quit.store(1, Ordering::Release);
        block.request.fetch_add(1, Ordering::AcqRel);
        shm::wake(&block.request);
        let _ = running.audio.join();
        let mut instance = running.instance;
        instance.close_editor();
        instance.idle();
        drop(instance);
    }
    let _ = out.flush();
    ExitCode::SUCCESS
}

fn handle(running: &mut Option<Running>, request: Request) -> Reply {
    let error = |what: String| Reply::Error(what);
    if let Request::Create {
        format,
        file,
        id,
        settings,
        shm,
    } = request
    {
        if running.is_some() {
            return error("an instance runs already".into());
        }
        // Another format is another backend here, beside CLAP.
        match format {
            PluginFormat::Clap => {}
        }
        let builtin = file.is_none();
        let mut backend = ClapBackend::files(file.map(PathBuf::from).into_iter().collect(), builtin);
        backend.scan();
        let mut instance = match backend.create(&id, settings) {
            Ok(instance) => instance,
            Err(e) => return error(e.to_string()),
        };
        let shared = match Shared::open(std::path::Path::new(&shm)) {
            Ok(shared) => Arc::new(shared),
            Err(e) => return error(format!("the shared block: {e}")),
        };
        let Some(device) = instance.take_processor() else {
            return error("no processor".into());
        };
        let audio = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("mdaw-plugin-audio".into())
                .spawn(move || serve_audio(device, &shared))
                .expect("starting the audio thread")
        };
        let reply = Reply::Created {
            info: instance.info().clone(),
            has_editor: instance.has_editor(),
            param_count: instance.param_count() as u32,
        };
        *running = Some(Running {
            _backend: backend,
            instance,
            shared,
            audio,
        });
        return reply;
    }
    let Some(running) = running.as_mut() else {
        return error("no instance yet".into());
    };
    let instance = &mut running.instance;
    match request {
        Request::Create { .. } | Request::Quit => Reply::Done,
        Request::Params { limit } => {
            Reply::Params(instance.params(limit as usize), instance.param_count() as u32)
        }
        Request::SetParam { id, value } => {
            instance.set_param(id, value);
            Reply::Done
        }
        Request::SaveState => match instance.save_state() {
            Ok(bytes) => Reply::State(bytes),
            Err(e) => error(e.to_string()),
        },
        Request::LoadState(bytes) => match instance.load_state(&bytes) {
            Ok(()) => Reply::Done,
            Err(e) => error(e.to_string()),
        },
        Request::OpenEditor { title } => match instance.open_editor(&title) {
            Ok(()) => Reply::Done,
            Err(e) => error(e.to_string()),
        },
        Request::CloseEditor => {
            instance.close_editor();
            Reply::Done
        }
    }
}

/// Asks for the scheduling of an audio thread, as the DAW's own has: run first, ahead of
/// everything that is not. Quietly nothing where the system does not allow it.
fn real_time() {
    // SAFETY: plain calls about the calling thread.
    unsafe {
        let param = libc::sched_param { sched_priority: 20 };
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
    }
}

/// Processes every block the DAW asks for, until told to stop.
fn serve_audio(mut device: Box<dyn ExternalDevice>, shared: &Shared) {
    real_time();
    let block = shared.block();
    // From the last block finished, not the last asked for: a block the DAW asked for before
    // this thread got going is still to be done.
    let mut last = block.done.load(Ordering::Acquire);
    loop {
        shm::wait(&block.request, last, Duration::from_millis(100));
        if block.quit.load(Ordering::Acquire) != 0 {
            break;
        }
        let seq = block.request.load(Ordering::Acquire);
        if seq == last {
            continue;
        }
        // SAFETY: a new `request`: the helper's turn, until it raises `done`.
        let data = unsafe { block.data() };
        let events = (data.events_len as usize).min(data.events.len());
        for event in &data.events[..events] {
            match event.kind {
                NOTE_ON => device.note_on(
                    event.offset,
                    event.key,
                    event.pitch.min(127) as u8,
                    event.vel.min(127) as u8,
                ),
                NOTE_OFF => device.note_off(event.offset, event.key),
                _ => {}
            }
        }
        let frames = (data.frames as usize).min(MAX_BLOCK);
        let time = data.time();
        let (left, right) = (&mut data.left, &mut data.right);
        device.process(&mut left[..frames], &mut right[..frames], &time);
        block.done.store(seq, Ordering::Release);
        shm::wake(&block.done);
        last = seq;
    }
    device.retire();
}
