//! The `mdaw` desktop client.

use mdaw_client::{DawApp, Session};
use mdaw_engine::Engine;

// Debug builds stop on any allocation made inside the audio callback.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: mdaw_engine::alloc_guard::GuardedAlloc = mdaw_engine::alloc_guard::GuardedAlloc;

struct Shell {
    app: DawApp,
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

fn main() -> eframe::Result {
    #[cfg(debug_assertions)]
    mdaw_engine::alloc_guard::set_abort(true);

    let engine = Engine::open().unwrap_or_else(|err| {
        eprintln!("mdaw: no audio output ({err}); running without sound");
        Engine::manual(48_000, true)
    });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mdaw")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mdaw",
        options,
        Box::new(|cc| {
            DawApp::setup(&cc.egui_ctx);
            Ok(Box::new(Shell {
                app: DawApp::new(Session::starter(engine)),
            }))
        }),
    )
}
