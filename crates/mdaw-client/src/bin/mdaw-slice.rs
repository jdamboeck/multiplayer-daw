//! A client without a window: a [`Session`] on Tracktion Engine, driven by commands on stdin,
//! one per line, answered on stdout with `RESULT key=value` lines and a final `OK <command>` or
//! `ERR <command>: <why>`. For the tests of the collaborative slice (`tests/slice.rs`), which
//! run several of these at once: Tracktion allows one engine per process.
//!
//! ```text
//! mdaw-slice --join <mdaw://...> --name Ann --profile <dir> [--no-plugins] [--device] [--scan CLAP,VST3,LV2]
//! ```
//!
//! Tracks and clips are named by their names, which the commands give them; notes by their
//! index in their clip, ordered by start and pitch. Between commands the session keeps running.
//!
//! Commands: `wait-scan`, `settle`, `wait-seq <n>`, `seq`, `doc-hash`,
//! `add-track <name>`, `pocket-param <track> <param> <value>`, `add-clip <track> <clip> <beat> <beats>`,
//! `add-notes <track> <clip> <beat>:<pitch>[,...]`, `set-note <track> <clip> <index> pitch|vel <value>`,
//! `note-pitch <track> <clip> <index>`, `add-plugin <track> clap:<id>`,
//! `plugin-param <track> <param id> <value>`, `shown-param <track> <param id>`,
//! `watch <track>`, `quiet-param <track> <param id> <normalised value>`,
//! `plugin-state <track>`, `save-states`,
//! `device-status <track>`, `render <file> <from tick> <to tick>`, `play`, `stop`, `seek <tick>`,
//! `state`, `sleep <ms>`, `notices`, `crash`, `quit`.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_client::outbox::Outbox;
use mdaw_client::{Session, Status, profile};
use mdaw_model::{
    BuiltinKind, Clip, ClipId, Device, DeviceKind, Note, NoteId, NoteProp, Op, PluginFormat, Project, Tick,
    TrackContent, TrackId, TrackShell,
};
use mdaw_net::JoinInfo;

const WAIT: Duration = Duration::from_secs(60);

struct Args {
    join: JoinInfo,
    name: String,
    profile: PathBuf,
    plugins: bool,
    device: bool,
    scan: Vec<PluginFormat>,
}

fn parse() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut join, mut name, mut profile) = (None, None, None);
    let (mut plugins, mut device, mut scan) = (true, false, vec![PluginFormat::Clap]);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--join" => join = Some(value()?.parse::<JoinInfo>().map_err(|e| e.to_string())?),
            "--name" => name = Some(value()?),
            "--profile" => profile = Some(PathBuf::from(value()?)),
            "--no-plugins" => plugins = false,
            "--device" => device = true,
            "--scan" => {
                scan = value()?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| PluginFormat::from_name(s).ok_or_else(|| format!("no plugin format {s}")))
                    .collect::<Result<_, _>>()?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        join: join.ok_or("--join is needed")?,
        name: name.ok_or("--name is needed")?,
        profile: profile.ok_or("--profile is needed")?,
        plugins,
        device,
        scan,
    })
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("mdaw-slice: {err}");
            return ExitCode::from(2);
        }
    };
    let started = Instant::now();
    let mut options = TracktionOptions::new(args.profile.join("engine"));
    options.open_audio_device = args.device;
    options.enable_plugins = args.plugins;
    options.scan = args.scan.clone();
    let audio = match TracktionAudio::new(options) {
        Ok(audio) => audio,
        Err(err) => {
            eprintln!("mdaw-slice: the engine did not start: {err}");
            return ExitCode::FAILURE;
        }
    };
    let who = match profile::open(&args.profile, Some(&args.name)) {
        Ok(who) => who,
        Err(err) => {
            eprintln!("mdaw-slice: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let outbox = Outbox::dir_for(&args.profile, &args.join);
    let session = match Session::join_with_outbox(args.join, who, &outbox, audio, Arc::new(|| {})) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("mdaw-slice: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    println!("RESULT started_ms={}", started.elapsed().as_millis());
    let mut driver = Driver { session };
    driver.say_status();

    let lines = stdin_lines();
    loop {
        let line = match lines.try_recv() {
            Ok(line) => line,
            Err(TryRecvError::Empty) => {
                driver.session.frame();
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(TryRecvError::Disconnected) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "quit" {
            println!("OK quit");
            break;
        }
        match driver.command(&line) {
            Ok(()) => println!("OK {line}"),
            Err(err) => println!("ERR {line}: {err}"),
        }
    }
    ExitCode::SUCCESS
}

fn stdin_lines() -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

struct Driver {
    session: Session,
}

type Res = Result<(), String>;

fn num<T: std::str::FromStr>(word: Option<&&str>, what: &str) -> Result<T, String> {
    word.ok_or_else(|| format!("{what} is missing"))?
        .parse()
        .map_err(|_| format!("{what} is not a number"))
}

fn ticks(beats: f64) -> Tick {
    Tick((beats * mdaw_model::PPQ as f64).round() as i64)
}

impl Driver {
    fn audio(&mut self) -> &mut TracktionAudio {
        self.session
            .audio_mut()
            .as_any_mut()
            .downcast_mut::<TracktionAudio>()
            .expect("a Tracktion session")
    }

    fn until(&mut self, what: &str, done: impl Fn(&Session) -> bool) -> Res {
        let deadline = Instant::now() + WAIT;
        loop {
            self.session.frame();
            if done(&self.session) {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!("timed out waiting until {what}"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn say_status(&self) {
        println!(
            "RESULT seq={} unconfirmed={} online={}",
            self.session.seq(),
            self.session.unconfirmed(),
            self.session.status() == Status::Online
        );
    }

    fn track(&self, name: &str) -> Result<TrackId, String> {
        self.session
            .project()
            .ordered_tracks()
            .find(|(_, t)| t.shell.name == name)
            .map(|(id, _)| id)
            .ok_or_else(|| format!("no track {name}"))
    }

    fn clip(&self, track: TrackId, name: &str) -> Result<ClipId, String> {
        let t = self.session.project().track(track).ok_or("no such track")?;
        t.content
            .clips
            .iter()
            .find(|(_, c)| c.name == name)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no clip {name}"))
    }

    /// The notes of a clip by start and pitch.
    fn notes(&self, track: TrackId, clip: ClipId) -> Vec<(NoteId, Note)> {
        let Some(c) = self
            .session
            .project()
            .track(track)
            .and_then(|t| t.content.clips.get(&clip))
        else {
            return Vec::new();
        };
        let mut notes: Vec<(NoteId, Note)> = c.notes.iter().map(|(id, n)| (*id, *n)).collect();
        notes.sort_by_key(|(id, n)| (n.start, n.pitch, *id));
        notes
    }

    fn edit(&mut self, ops: Vec<Op>) -> Res {
        if self.session.edit(ops) {
            Ok(())
        } else {
            Err("the edit changed nothing or was refused".into())
        }
    }

    fn plugin_device(&self, track: TrackId) -> Result<mdaw_model::DeviceId, String> {
        let t = self.session.project().track(track).ok_or("no such track")?;
        t.content
            .devices
            .iter()
            .find(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
            .map(|d| d.id)
            .ok_or_else(|| "no plugin on the track".into())
    }

    fn command(&mut self, line: &str) -> Res {
        let words: Vec<&str> = line.split_whitespace().collect();
        let arg = |i: usize| {
            words
                .get(i)
                .copied()
                .ok_or_else(|| format!("argument {i} is missing"))
        };
        match words[0] {
            "wait-scan" => self.until("the plugins are scanned", |s| !s.scanning_plugins()),
            "settle" => {
                self.until("all edits are confirmed", |s| {
                    s.unconfirmed() == 0 && s.status() == Status::Online
                })?;
                self.say_status();
                Ok(())
            }
            "wait-seq" => {
                let seq: u64 = num(words.get(1), "seq")?;
                self.until("the seq is reached", |s| {
                    s.seq() >= seq && s.unconfirmed() == 0 && s.status() == Status::Online
                })?;
                self.say_status();
                Ok(())
            }
            "seq" => {
                self.say_status();
                Ok(())
            }
            "doc-hash" => {
                let bytes = postcard::to_stdvec(self.session.project()).map_err(|e| e.to_string())?;
                let hash = mdaw_proto::blob_hash(&bytes);
                let hex: String = hash.0.iter().map(|b| format!("{b:02x}")).collect();
                println!("RESULT doc_hash={hex}");
                Ok(())
            }
            "add-track" => {
                let name = arg(1)?;
                let ids = self.session.ids();
                let (id, device) = (ids.track(), ids.device());
                let mut content = TrackContent::default();
                let kind = DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    dsp_version: mdaw_dsp::pocket::DSP_VERSION,
                };
                content.devices.push_back(Device::new(device, kind));
                let after = self.session.project().track_order.last().copied();
                self.edit(vec![Op::TrackAdd {
                    id,
                    shell: TrackShell {
                        name: name.into(),
                        color: 0,
                    },
                    content,
                    after,
                }])
            }
            "pocket-param" => {
                let track = self.track(arg(1)?)?;
                let param: u32 = num(words.get(2), "param")?;
                let value: f64 = num(words.get(3), "value")?;
                let device = self
                    .session
                    .project()
                    .track(track)
                    .and_then(|t| {
                        t.content
                            .devices
                            .iter()
                            .find(|d| matches!(d.kind, DeviceKind::Builtin { .. }))
                    })
                    .map(|d| d.id)
                    .ok_or("no built-in device")?;
                self.edit(vec![Op::DeviceSetParam {
                    track,
                    id: device,
                    param,
                    value: Some(value),
                }])
            }
            "add-clip" => {
                let track = self.track(arg(1)?)?;
                let name = arg(2)?;
                let start: f64 = num(words.get(3), "start")?;
                let length: f64 = num(words.get(4), "length")?;
                let id = self.session.ids().clip();
                self.edit(vec![Op::ClipAdd {
                    track,
                    id,
                    clip: Clip {
                        start: ticks(start),
                        length: ticks(length),
                        name: name.into(),
                        notes: Default::default(),
                    },
                }])
            }
            "add-notes" => {
                let track = self.track(arg(1)?)?;
                let clip = self.clip(track, arg(2)?)?;
                let mut ops = Vec::new();
                for item in arg(3)?.split(',') {
                    let (beat, pitch) = item.split_once(':').ok_or("notes are <beat>:<pitch>")?;
                    let beat: f64 = beat.parse().map_err(|_| "beat is not a number")?;
                    let pitch: u8 = pitch.parse().map_err(|_| "pitch is not a number")?;
                    ops.push(Op::NoteAdd {
                        track,
                        clip,
                        id: self.session.ids().note(),
                        note: Note {
                            start: ticks(beat),
                            dur: ticks(0.5),
                            pitch,
                            vel: 100,
                        },
                    });
                }
                self.edit(ops)
            }
            "set-note" => {
                let track = self.track(arg(1)?)?;
                let clip = self.clip(track, arg(2)?)?;
                let index: usize = num(words.get(3), "index")?;
                let value: u8 = num(words.get(5), "value")?;
                let (id, _) = *self.notes(track, clip).get(index).ok_or("no such note")?;
                let prop = match arg(4)? {
                    "pitch" => NoteProp::Pitch(value),
                    "vel" => NoteProp::Vel(value),
                    other => return Err(format!("no note property {other}")),
                };
                self.edit(vec![Op::NoteSet {
                    track,
                    clip,
                    id,
                    prop,
                }])
            }
            "note-pitch" => {
                let track = self.track(arg(1)?)?;
                let clip = self.clip(track, arg(2)?)?;
                let index: usize = num(words.get(3), "index")?;
                let (_, note) = *self.notes(track, clip).get(index).ok_or("no such note")?;
                println!("RESULT pitch={}", note.pitch);
                Ok(())
            }
            "add-plugin" => {
                let track = self.track(arg(1)?)?;
                let (format, id) = arg(2)?.split_once(':').ok_or("plugins are <format>:<id>")?;
                let format = PluginFormat::from_name(&format.to_uppercase()).ok_or("no such format")?;
                let entry = self
                    .session
                    .plugin_catalog()
                    .into_iter()
                    .find(|p| p.format == format && p.id == id)
                    .ok_or_else(|| format!("{id} is not installed here"))?;
                if self.session.add_plugin(track, &entry) {
                    Ok(())
                } else {
                    Err("the edit was refused".into())
                }
            }
            "plugin-param" => {
                // As the device tab's generic slider does: the plugin's parameter is set in the
                // engine, and its state reaches the document by the capture path.
                let track = self.track(arg(1)?)?;
                let param = arg(2)?.to_string();
                let value: f64 = num(words.get(3), "value")?;
                let device = self.plugin_device(track)?;
                self.until("the plugin runs", |s| s.plugin_running(track, device))?;
                let deadline = Instant::now() + WAIT;
                loop {
                    let found = self
                        .session
                        .plugin_params(track, device, usize::MAX)
                        .is_some_and(|(list, _)| list.iter().any(|p| p.id == param));
                    if found {
                        break;
                    }
                    if Instant::now() > deadline {
                        return Err(format!("the plugin has no parameter {param}"));
                    }
                    self.session.frame();
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.session.set_plugin_param(track, device, &param, value);
                Ok(())
            }
            "watch" => {
                // Looks at the plugin's settings as its open window would make the client do.
                let track = self.track(arg(1)?)?;
                let device = self.plugin_device(track)?;
                let audio = self.session.audio_mut();
                let te = audio
                    .as_any_mut()
                    .downcast_mut::<TracktionAudio>()
                    .ok_or("not on Tracktion")?;
                te.watch(device);
                Ok(())
            }
            "quiet-param" => {
                // Changes a parameter in the engine without the plugin reporting it, as a plugin
                // that loads a preset without a word to the host.
                let track = self.track(arg(1)?)?;
                let param = arg(2)?.to_string();
                let value: f32 = num(words.get(3), "value")?;
                let device = self.plugin_device(track)?;
                let audio = self.session.audio_mut();
                let te = audio
                    .as_any_mut()
                    .downcast_mut::<TracktionAudio>()
                    .ok_or("not on Tracktion")?;
                te.engine()
                    .device_set_param(device.0.0, &param, value)
                    .map_err(|e| e.message)?;
                Ok(())
            }
            "shown-param" => {
                // What the device tab shows for a parameter right now: the reading it has, not
                // a new one (asking also starts a new reading, which this does not wait for).
                let track = self.track(arg(1)?)?;
                let param = arg(2)?.to_string();
                let device = self.plugin_device(track)?;
                // The first look has nothing to show yet: it starts the reading, waited for here.
                let deadline = Instant::now() + WAIT;
                loop {
                    let shown = self
                        .session
                        .plugin_params(track, device, usize::MAX)
                        .and_then(|(list, _)| list.into_iter().find(|p| p.id == param));
                    if let Some(entry) = shown {
                        println!("RESULT shown={}", entry.value);
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        return Err(format!("the plugin shows no parameter {param}"));
                    }
                    self.session.frame();
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            "plugin-state" => {
                let track = self.track(arg(1)?)?;
                let device = self.plugin_device(track)?;
                let state = self
                    .session
                    .project()
                    .track(track)
                    .and_then(|t| t.content.device(device))
                    .and_then(|d| match &d.kind {
                        DeviceKind::Plugin { state, .. } => *state,
                        DeviceKind::Builtin { .. } => None,
                    });
                let text: String = state.map_or("none".into(), |h| {
                    h.0.iter().take(8).map(|b| format!("{b:02x}")).collect()
                });
                println!("RESULT plugin_state={text}");
                Ok(())
            }
            "save-states" => {
                let at = Instant::now();
                self.session.save_plugin_states();
                println!("RESULT save_states_ms={}", at.elapsed().as_millis());
                Ok(())
            }
            "device-status" => {
                let track = self.track(arg(1)?)?;
                self.session.frame();
                self.audio().wait_idle();
                let project: Project = self.session.project().clone();
                let devices = project
                    .track(track)
                    .ok_or("no such track")?
                    .content
                    .devices
                    .clone();
                for device in devices {
                    let status = self.session.device_status(track, device.id);
                    println!("RESULT device_status={status:?}");
                }
                println!("RESULT track_sound={:?}", self.session.track_sound(track));
                Ok(())
            }
            "render" => {
                let path = PathBuf::from(arg(1)?);
                let from: i64 = num(words.get(2), "from")?;
                let to: i64 = num(words.get(3), "to")?;
                self.session.frame();
                self.audio().wait_idle();
                self.session
                    .render(&path, Tick(from), Tick(to))
                    .map_err(|e| format!("render failed: {e}"))?;
                println!("RESULT rendered={}", path.display());
                Ok(())
            }
            "play" => {
                self.session.play();
                self.audio().wait_idle();
                Ok(())
            }
            "stop" => {
                self.session.pause();
                self.audio().wait_idle();
                Ok(())
            }
            "seek" => {
                let tick: i64 = num(words.get(1), "tick")?;
                self.session.seek(Tick(tick));
                self.audio().wait_idle();
                Ok(())
            }
            "state" => {
                self.session.frame();
                self.audio().wait_idle();
                let state = self.audio().engine().state();
                println!(
                    "RESULT playing={} engine_playing={} position_tick={} device_open={}",
                    self.session.is_playing(),
                    state.playing,
                    state.position_tick,
                    state.device_open
                );
                Ok(())
            }
            "sleep" => {
                let ms: u64 = num(words.get(1), "ms")?;
                let until = Instant::now() + Duration::from_millis(ms);
                while Instant::now() < until {
                    self.session.frame();
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            }
            "notices" => {
                for notice in self.session.notices() {
                    println!("RESULT notice={notice}");
                }
                Ok(())
            }
            "crash" => {
                // As a crash: nothing is written on the way out.
                std::process::abort();
            }
            other => Err(format!("unknown command {other}")),
        }
    }
}
