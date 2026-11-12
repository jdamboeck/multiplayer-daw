//! Builds the Tracktion bridge (`native/`) with CMake and tells rustc how to link it.
//!
//! The build directory defaults to `<workspace>/target/native`, the one the manual CMake commands
//! in docs/tracktion-integration.md use, so the long JUCE/Tracktion build is shared between the
//! two. `MDAW_NATIVE_BUILD_DIR` overrides it (relative paths are taken from the workspace root).
//! The directory is configured only when it has no `CMakeCache.txt`; after that, `cmake --build`
//! runs every time this script runs, and is a no-op when nothing changed.
//!
//! CMake is driven with `std::process::Command` rather than the `cmake` crate: that crate builds
//! into `OUT_DIR` (a fresh 10-minute build per profile and per `cargo clean`) and runs the
//! `install` step, which this project does not have.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BUILD_DIR_ENV: &str = "MDAW_NATIVE_BUILD_DIR";

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <workspace>/crates/")
        .to_path_buf();
    let native = workspace.join("native");

    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        native.join("CMakeLists.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native.join("tracktion-bridge").display()
    );
    println!("cargo:rerun-if-changed={}", native.join("clap-format").display());
    println!("cargo:rerun-if-env-changed={BUILD_DIR_ENV}");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        fail(&format!(
            "the Tracktion bridge is not supported on {target_os} yet: only Linux builds it so far. \
             Build without mdaw-tracktion (plain `cargo build` leaves it out)."
        ));
    }

    if !native.join("third_party/tracktion_engine/modules").is_dir() {
        fail(
            "Tracktion Engine and JUCE are missing (native/third_party/tracktion_engine/modules). \
             Fetch the pinned submodules from the repository root with:\n\n    \
             git -c url.\"https://github.com/\".insteadOf=\"git@github.com:\" \
             submodule update --init --recursive --depth 1\n\n\
             (see docs/tracktion-integration.md, section \"Build\").",
        );
    }

    let build = match env::var_os(BUILD_DIR_ENV) {
        Some(dir) if !dir.is_empty() => workspace.join(dir),
        _ => workspace.join("target").join("native"),
    };

    if !build.join("CMakeCache.txt").is_file() {
        let mut configure = Command::new("cmake");
        configure
            .arg("-S")
            .arg(&native)
            .arg("-B")
            .arg(&build)
            .arg("-DCMAKE_BUILD_TYPE=RelWithDebInfo");
        if on_path("ninja") {
            configure.args(["-G", "Ninja"]);
        }
        run(configure, "configure the native build");
    }

    // The bridge archive, and the CLAP plugin format it links (a separate archive in CMake).
    let mut compile = Command::new("cmake");
    compile.arg("--build").arg(&build).args([
        "--parallel",
        "--target",
        "mdaw_tracktion_bridge",
        "mdaw_clap_format",
    ]);
    run(
        compile,
        "build the native library (the first build takes about ten minutes)",
    );

    // The libraries in the order of the C test's link line (target/native/build.ninja,
    // `mdaw-bridge-test`): the bridge, then the CLAP format. Both archives carry copies of the
    // JUCE module objects; the first one to define a symbol wins, as for the C test. `-bundle`
    // keeps the ~300 MB archives out of the rlib and hands them to the final link instead.
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("tracktion-bridge").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("clap-format").display()
    );
    println!("cargo:rustc-link-lib=static:-bundle=mdaw_tracktion_bridge");
    println!("cargo:rustc-link-lib=static:-bundle=mdaw_clap_format");
    // System libraries from the same link line. JUCE links ALSA directly (JACK, X11 and the
    // rest are opened at run time); fontconfig and freetype come from juce_graphics; atomic
    // from std::atomic on wide types. dl, pthread and rt are empty stubs in glibc 2.34 and
    // later, kept for older systems. Then the C++ runtime the bridge is written against.
    for lib in [
        "asound",
        "fontconfig",
        "freetype",
        "dl",
        "pthread",
        "rt",
        "atomic",
        "stdc++",
        "m",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    println!("cargo:root={}", build.display());
}

fn run(mut command: Command, what: &str) {
    // CMake's own output goes to stderr: cargo shows it when the build fails, and nothing in it
    // can be mistaken for a `cargo:` directive.
    command.stdout(Stdio::from(std::io::stderr()));
    eprintln!("running {command:?}");
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => fail(&format!("could not {what}: {command:?} exited with {status}")),
        Err(error) => fail(&format!(
            "could not {what}: {command:?} did not start ({error}). CMake, a C++ compiler and the \
             system libraries listed in docs/tracktion-integration.md are needed."
        )),
    }
}

fn on_path(program: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    panic!("mdaw-tracktion-sys: {message}");
}
