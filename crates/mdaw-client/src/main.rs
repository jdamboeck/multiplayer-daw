//! The `mdaw` desktop client.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_client::outbox::Outbox;
use mdaw_client::{Audio, DawApp, Session, SilentAudio, Waker, profile};
use mdaw_net::JoinInfo;

const USAGE: &str = "\
mdaw, a collaborative DAW (prototype)

Usage: mdaw [--join <mdaw://...>] [--name <name>] [--profile <dir>] [--no-plugins]

  --join <string>   Join the project on a server. The server prints the join string when it starts.
                    Without it, mdaw opens a local project that is not saved.
  --name <name>     The name others see. Remembered in the profile.
  --profile <dir>   Where this install keeps its identity, and the edits a server has not
                    confirmed yet. Use different directories to run several clients as
                    different people on one machine.
  --no-plugins      Load no plugins (also --no-clap). Tracks with plugins are silent.
  -h, --help        Show this text.

mdaw plays through Tracktion Engine, which hosts VST3, LV2 and CLAP plugins. They are looked
for in the usual folders (CLAP also in CLAP_PATH). If the engine does not start, mdaw says why
and runs without sound.
";

#[derive(Default)]
struct Args {
    join: Option<JoinInfo>,
    name: Option<String>,
    profile: Option<PathBuf>,
    no_plugins: bool,
}

/// `Ok(None)` means help was asked for.
fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--join" => {
                let text = value("a join string")?;
                parsed.join = Some(text.parse().map_err(|err| format!("not a join string: {err}"))?);
            }
            "--name" => parsed.name = Some(value("a name")?),
            "--profile" => parsed.profile = Some(value("a directory")?.into()),
            "--no-plugins" | "--no-clap" => parsed.no_plugins = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(parsed))
}

struct Shell {
    app: DawApp,
}

impl eframe::App for Shell {
    /// Runs before every frame, and also while the window is hidden or covered: the session's
    /// work happens here, so it goes on while nothing is drawn.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic();
        if let Some(wait) = self.app.session.next_frame_in() {
            ctx.request_repaint_after(wait);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("mdaw: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let profile_dir = args.profile.clone().or_else(profile::default_dir);
    let Some(engine_dir) = profile_dir.as_ref().map(|dir| dir.join("engine")) else {
        eprintln!("mdaw: no place for the profile; pass --profile <dir>");
        return ExitCode::FAILURE;
    };
    let audio = tracktion_audio(&engine_dir, !args.no_plugins).unwrap_or_else(|err| {
        eprintln!("mdaw: {err}; running without sound");
        Box::new(SilentAudio::new())
    });
    let engine_failed = audio.as_any().is::<SilentAudio>();

    // The network thread wakes the UI through this once the window exists.
    let ctx: Arc<OnceLock<egui::Context>> = Arc::default();
    let mut title = "mdaw".to_string();
    let session = match args.join {
        None => Session::starter(audio),
        Some(join) => {
            let Some(dir) = profile_dir else {
                eprintln!("mdaw: no place for the profile; pass --profile <dir>");
                return ExitCode::FAILURE;
            };
            let who = match profile::open(&dir, args.name.as_deref()) {
                Ok(who) => who,
                Err(err) => {
                    eprintln!("mdaw: {err:#}");
                    return ExitCode::FAILURE;
                }
            };
            title = format!("mdaw - {} @ {}:{}", who.name, join.host, join.port);
            mdaw_client::trace::set_name(&who.name);
            let waker: Waker = {
                let ctx = ctx.clone();
                Arc::new(move || {
                    if let Some(ctx) = ctx.get() {
                        ctx.request_repaint();
                    }
                })
            };
            let outbox = Outbox::dir_for(&dir, &join);
            match Session::join_with_outbox(join, who, &outbox, audio, waker) {
                Ok(session) => session,
                Err(err) => {
                    eprintln!("mdaw: {err:#}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let mut session = session;
    if engine_failed {
        session.notify("The audio engine did not start, so mdaw plays nothing; see the terminal for why.");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    let run = eframe::run_native(
        "mdaw",
        options,
        Box::new(move |cc| {
            DawApp::setup(&cc.egui_ctx);
            let _ = ctx.set(cc.egui_ctx.clone());
            let mut app = DawApp::new(session);
            app.enable_midi();
            Ok(Box::new(Shell { app }))
        }),
    );
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mdaw: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Tracktion Engine on the default audio device, its settings in `dir`. Plugins are looked
/// for on a thread of the engine's while the window opens.
fn tracktion_audio(dir: &std::path::Path, plugins: bool) -> Result<Box<dyn Audio>, String> {
    let mut options = TracktionOptions::new(dir);
    options.open_audio_device = true;
    options.enable_plugins = plugins;
    let audio = TracktionAudio::new(options).map_err(|e| format!("the audio engine did not start: {e}"))?;
    let state = audio.engine().state();
    if state.device_open {
        eprintln!(
            "mdaw: Tracktion Engine, audio device open at {} Hz, {} frames",
            state.sample_rate, state.block_size
        );
    } else {
        eprintln!("mdaw: Tracktion Engine, no audio device opened; running without sound");
    }
    Ok(Box::new(audio))
}
