//! Developer commands: `cargo xtask <command>`.
//!
//! Everything a demo needs lives under `target/demo`: the server's data, the join string it
//! printed, and one profile directory per client name.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

const USAGE: &str = "\
Usage: cargo xtask <command> [options]

  demo [names...]   Start a server and one client per name (default: Ann Ben).
                    Ends when the last client window is closed.
  server            Start only the server, in the foreground. Ctrl+C stops it.
  client <name>     Start one client that joins the running server as <name>.

Options:
  --fresh           Forget the demo project and all profiles first (demo, server).
  --listen <addr>   Where the server listens (default: 127.0.0.1:7464).
  --release         Use optimised builds.
  --no-plugins      Clients load no plugins; tracks with plugins are silent in them.
  --test-synth      Clients also offer the built-in test synth.

The pieces can be mixed: `cargo xtask server` in one terminal and `cargo xtask client Ann` in
others gives the same setup as `demo`, with a server that can be stopped and started alone.
";

type Result<T> = std::result::Result<T, String>;

struct Options {
    names: Vec<String>,
    fresh: bool,
    release: bool,
    listen: String,
    /// Clients load no plugins.
    no_plugins: bool,
    /// Clients offer the test synth.
    test_synth: bool,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let mut options = Options {
        names: Vec::new(),
        fresh: false,
        release: false,
        listen: "127.0.0.1:7464".into(),
        no_plugins: false,
        test_synth: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fresh" => options.fresh = true,
            "--release" => options.release = true,
            "--no-plugins" | "--no-clap" => options.no_plugins = true,
            "--test-synth" => options.test_synth = true,
            "--listen" => match args.next() {
                Some(addr) => options.listen = addr,
                None => return fail("--listen needs an address"),
            },
            flag if flag.starts_with('-') => return fail(&format!("unknown option {flag}")),
            _ => options.names.push(arg),
        }
    }
    let run = match command.as_str() {
        "demo" => demo(options),
        "server" => server(options),
        "client" => client(options),
        "" | "help" | "-h" | "--help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => return fail(&format!("unknown command {other}")),
    };
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn fail(what: &str) -> ExitCode {
    eprintln!("xtask: {what}\n\n{USAGE}");
    ExitCode::from(2)
}

fn demo(mut options: Options) -> Result<()> {
    if options.names.is_empty() {
        options.names = vec!["Ann".into(), "Ben".into()];
    }
    build(options.release)?;
    let (mut server, join) = start_server(&options)?;
    let clients: Result<Vec<Child>> = options
        .names
        .iter()
        .map(|name| start_client(name, &join, &options))
        .collect();
    let outcome = clients.and_then(|clients| {
        let mut failed = 0;
        for mut client in clients {
            failed += usize::from(!client.wait().is_ok_and(|status| status.success()));
        }
        if failed == 0 {
            Ok(())
        } else {
            Err(format!("{failed} client(s) ended with an error"))
        }
    });
    let _ = server.kill();
    let _ = server.wait();
    outcome
}

fn server(options: Options) -> Result<()> {
    if !options.names.is_empty() {
        return Err("server takes no names".into());
    }
    build(options.release)?;
    let (mut server, join) = start_server(&options)?;
    println!("{join}");
    eprintln!("xtask: in another terminal: cargo xtask client <name>");
    let status = server
        .wait()
        .map_err(|err| format!("waiting for the server: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the server ended with {status}"))
    }
}

fn client(options: Options) -> Result<()> {
    let [name] = options.names.as_slice() else {
        return Err("client takes exactly one name".into());
    };
    if options.fresh {
        return Err("--fresh belongs to the server; clients only join".into());
    }
    let path = demo_dir().join("join.txt");
    let join = std::fs::read_to_string(&path)
        .map_err(|_| format!("no {}; start `cargo xtask server` first", path.display()))?;
    build(options.release)?;
    let status = start_client(name, join.trim(), &options)?
        .wait()
        .map_err(|err| format!("waiting for the client: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the client ended with {status}"))
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace")
        .to_path_buf()
}

fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => root().join(dir),
        _ => root().join("target"),
    }
}

fn demo_dir() -> PathBuf {
    target_dir().join("demo")
}

fn binary(name: &str, release: bool) -> PathBuf {
    let profile = if release { "release" } else { "debug" };
    target_dir()
        .join(profile)
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn build(release: bool) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root()).args([
        "build",
        "-p",
        "mdaw-server",
        "-p",
        "mdaw-client",
        "-p",
        "mdaw-plugin",
    ]);
    // Cargo describes xtask itself to this process. Passed on, that description makes build
    // scripts that watch it (ring's does) rebuild on every switch between xtask and plain cargo.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        let about_xtask = name.starts_with("CARGO_PKG_")
            || [
                "CARGO_MANIFEST_DIR",
                "CARGO_MANIFEST_PATH",
                "CARGO_CRATE_NAME",
                "CARGO_BIN_NAME",
                "CARGO_PRIMARY_PACKAGE",
            ]
            .contains(&name.as_str());
        if about_xtask {
            command.env_remove(name);
        }
    }
    if release {
        command.arg("--release");
    }
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("the build failed with {status}")),
        Err(err) => Err(format!("could not run cargo: {err}")),
    }
}

/// Starts the server and waits for its join string, which is also left in `join.txt`.
fn start_server(options: &Options) -> Result<(Child, String)> {
    let dir = demo_dir();
    if options.fresh && dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|err| format!("clearing {}: {err}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|err| format!("creating {}: {err}", dir.display()))?;

    let mut server = Command::new(binary("mdaw-server", options.release))
        .arg("--data")
        .arg(dir.join("server"))
        .args(["--listen", &options.listen])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not start the server: {err}"))?;
    let mut join = String::new();
    let stdout = server.stdout.take().expect("stdout is piped");
    let read = BufReader::new(stdout).read_line(&mut join);
    let join = join.trim().to_string();
    if read.is_err() || join.is_empty() {
        let _ = server.wait();
        return Err("the server did not start; see its message above".into());
    }
    if let Err(err) = std::fs::write(dir.join("join.txt"), &join) {
        let _ = server.kill();
        return Err(format!("writing join.txt: {err}"));
    }
    Ok((server, join))
}

fn start_client(name: &str, join: &str, options: &Options) -> Result<Child> {
    let profile = demo_dir().join("profiles").join(name.to_lowercase());
    let mut command = Command::new(binary("mdaw", options.release));
    command
        .args(["--join", join, "--name", name, "--profile"])
        .arg(profile);
    if options.no_plugins {
        command.arg("--no-plugins");
    }
    if options.test_synth {
        command.arg("--test-synth");
    }
    command
        .spawn()
        .map_err(|err| format!("could not start a client: {err}"))
}
