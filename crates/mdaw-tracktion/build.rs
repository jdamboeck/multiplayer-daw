//! Passes the path of the plugin scanner that `mdaw-tracktion-sys` built on to this crate as
//! `MDAW_TRACKTION_BUILT_SCANNER` (see `built_scanner_path`).

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_MDAW_TRACKTION_BRIDGE_SCANNER");
    if let Ok(scanner) = std::env::var("DEP_MDAW_TRACKTION_BRIDGE_SCANNER") {
        println!("cargo:rustc-env=MDAW_TRACKTION_BUILT_SCANNER={scanner}");
    }
}
