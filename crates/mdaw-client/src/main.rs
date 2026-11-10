//! The `mdaw` desktop client.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use mdaw_client::{DawApp, Session, Waker, profile};
use mdaw_engine::Engine;
use mdaw_net::JoinInfo;

// Debug builds stop on any allocation made inside the audio callback.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: mdaw_engine::alloc_guard::GuardedAlloc = mdaw_engine::alloc_guard::GuardedAlloc;

const USAGE: &str = "\
mdaw, a collaborative DAW (prototype)

Usage: mdaw [--join <mdaw://...>] [--name <name>] [--profile <dir>]

  --join <string>   Join the project on a server. The server prints the join string when it starts.
                    Without it, mdaw opens a local project that is not saved.
  --name <name>     The name others see. Remembered in the profile.
  --profile <dir>   Where this install keeps its identity. Use different directories to run
                    several clients as different people on one machine.
  -h, --help        Show this text.

With MDAW_SYNC_LOG=1 in the environment, a joined client prints once a second where its
playhead is against the server's clock.
";

#[derive(Default)]
struct Args {
    join: Option<JoinInfo>,
    name: Option<String>,
    profile: Option<PathBuf>,
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
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(parsed))
}

struct Shell {
    app: DawApp,
    /// When the sync log last wrote a line, if it is switched on.
    sync_logged: Option<Instant>,
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
        if let Some(last) = &mut self.sync_logged
            && last.elapsed() >= Duration::from_secs(1)
        {
            *last = Instant::now();
            log_sync(&self.app.session);
        }
        if self.sync_logged.is_some() {
            // A window with nothing to draw sleeps; the log goes on.
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }
}

/// One line on where this client is in the shared transport. Lines of several clients are
/// compared by their server time.
fn log_sync(session: &Session) {
    let Some(timing) = session.timing() else {
        return;
    };
    let ms = |micros: i64| micros as f64 / 1000.0;
    let drift = match timing.drift_micros {
        Some(drift) => format!("audio {:+.2} ms off the shared playhead", ms(drift)),
        None => "stopped".into(),
    };
    eprintln!(
        "sync: server time {:.3} s, playhead at tick {:.0}, {drift}, clock known to ±{:.2} ms, {} resyncs",
        timing.server_micros as f64 / 1e6,
        session.position(),
        ms(timing.rtt_micros) / 2.0,
        timing.resyncs,
    );
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

    #[cfg(debug_assertions)]
    mdaw_engine::alloc_guard::set_abort(true);

    let engine = Engine::open().unwrap_or_else(|err| {
        eprintln!("mdaw: no audio output ({err}); running without sound");
        Engine::manual(48_000, true)
    });

    // The network thread wakes the UI through this once the window exists.
    let ctx: Arc<OnceLock<egui::Context>> = Arc::default();
    let mut title = "mdaw".to_string();
    let session = match args.join {
        None => Session::starter(engine),
        Some(join) => {
            let Some(dir) = args.profile.or_else(profile::default_dir) else {
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
            let waker: Waker = {
                let ctx = ctx.clone();
                Arc::new(move || {
                    if let Some(ctx) = ctx.get() {
                        ctx.request_repaint();
                    }
                })
            };
            match Session::join(join, who, engine, waker) {
                Ok(session) => session,
                Err(err) => {
                    eprintln!("mdaw: {err:#}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

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
            let sync_log = std::env::var_os("MDAW_SYNC_LOG").is_some_and(|value| value != "0");
            Ok(Box::new(Shell {
                app: DawApp::new(session),
                sync_logged: sync_log.then(Instant::now),
            }))
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
