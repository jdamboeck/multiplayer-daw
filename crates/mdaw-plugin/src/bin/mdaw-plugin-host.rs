//! Runs one plugin for mdaw, in a process of its own; see `mdaw_plugin::isolated`.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    mdaw_plugin::isolated::child::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("mdaw-plugin-host: plugins run inside mdaw on this system");
    std::process::ExitCode::from(2)
}
