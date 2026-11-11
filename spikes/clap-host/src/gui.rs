//! Plugin GUI: report what the plugin supports, then try to show it.
//!
//! Linux: embed into a host-created X11 window (through XWayland on a Wayland session),
//! falling back to a plugin-owned floating window. Windows is not implemented in the spike.

use crate::host::Plugin;
use clack_extensions::gui::{GuiApiType, GuiConfiguration};
use std::path::Path;
use std::time::Duration;

pub fn report_support(plugin: &mut Plugin) {
    let Some(gui) = plugin.exts.gui else {
        println!("gui:        plugin has no gui extension");
        return;
    };
    let handle = plugin.instance.plugin_handle();
    let mut supported = Vec::new();
    for (name, api_type) in [
        ("x11", GuiApiType::X11),
        ("wayland", GuiApiType::WAYLAND),
        ("win32", GuiApiType::WIN32),
        ("cocoa", GuiApiType::COCOA),
    ] {
        for is_floating in [false, true] {
            if gui.is_api_supported(
                &handle,
                GuiConfiguration {
                    api_type,
                    is_floating,
                },
            ) {
                supported.push(format!(
                    "{name}/{}",
                    if is_floating { "floating" } else { "embedded" }
                ));
            }
        }
    }
    let preferred = gui
        .get_preferred_api(&handle)
        .map(|c| format!("{:?} floating={}", c.api_type, c.is_floating))
        .unwrap_or_else(|| "none stated".into());
    println!(
        "gui:        supports [{}], prefers {preferred}",
        supported.join(", ")
    );
}

#[cfg(not(target_os = "linux"))]
pub fn run(_: &mut Plugin, _: Duration, _: Option<&Path>, _: bool) -> Result<(), String> {
    Err("gui hosting is only implemented for Linux in this spike".into())
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use clack_extensions::gui::{GuiSize, PluginGui, Window};
    use std::collections::HashSet;
    use std::fmt::Display;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    fn s(e: impl Display) -> String {
        e.to_string()
    }

    pub fn run(
        plugin: &mut Plugin,
        duration: Duration,
        shot: Option<&Path>,
        force_floating: bool,
    ) -> Result<(), String> {
        let gui = plugin.exts.gui.ok_or("plugin has no gui extension")?;
        let embedded = GuiConfiguration {
            api_type: GuiApiType::X11,
            is_floating: false,
        };
        if !force_floating && gui.is_api_supported(&plugin.instance.plugin_handle(), embedded) {
            match x11rb::connect(None) {
                Ok((conn, screen)) => {
                    gui.create(&plugin.instance.plugin_handle(), embedded)
                        .map_err(s)?;
                    let result = embedded_x11(plugin, gui, &conn, screen, duration, shot);
                    gui.destroy(&plugin.instance.plugin_handle());
                    plugin.pump();
                    return result;
                }
                Err(e) => println!("gui: no X11 display ({e}); trying a floating window"),
            }
        }
        floating(plugin, gui, duration)
    }

    fn embedded_x11(
        plugin: &mut Plugin,
        gui: PluginGui,
        conn: &RustConnection,
        screen: usize,
        duration: Duration,
        shot: Option<&Path>,
    ) -> Result<(), String> {
        let screen = &conn.setup().roots[screen];
        let size = gui.get_size(&plugin.instance.plugin_handle()).unwrap_or(GuiSize {
            width: 800,
            height: 600,
        });
        let can_resize = gui.can_resize(&plugin.instance.plugin_handle());
        println!(
            "gui: embedded X11, plugin size {}x{}, resizable {can_resize}",
            size.width, size.height
        );

        let window = conn.generate_id().map_err(s)?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            size.width as u16,
            size.height as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::EXPOSURE),
        )
        .map_err(s)?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"mdaw spike: plugin window",
        )
        .map_err(s)?;
        if !can_resize {
            // A fixed-size plugin: tell the window manager, so tiling compositors float the
            // window at its natural size instead of stretching or clipping it.
            let fixed = Some((size.width as i32, size.height as i32));
            x11rb::properties::WmSizeHints {
                min_size: fixed,
                max_size: fixed,
                ..Default::default()
            }
            .set_normal_hints(conn, window)
            .map_err(s)?;
        }
        let wm_protocols = conn
            .intern_atom(false, b"WM_PROTOCOLS")
            .map_err(s)?
            .reply()
            .map_err(s)?
            .atom;
        let wm_delete = conn
            .intern_atom(false, b"WM_DELETE_WINDOW")
            .map_err(s)?
            .reply()
            .map_err(s)?
            .atom;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            wm_protocols,
            AtomEnum::ATOM,
            &[wm_delete],
        )
        .map_err(s)?;
        conn.flush().map_err(s)?;

        // SAFETY: `window` is a live X11 window and outlives the plugin GUI, which the caller
        // destroys before the connection is dropped.
        unsafe {
            gui.set_parent(
                &plugin.instance.plugin_handle(),
                Window::from_x11_handle(window as std::ffi::c_ulong),
            )
        }
        .map_err(|e| format!("set_parent: {e}"))?;
        conn.map_window(window).map_err(s)?;
        conn.flush().map_err(s)?;
        if let Err(e) = gui.show(&plugin.instance.plugin_handle()) {
            println!("gui: show() reported {e}");
        }

        let started = Instant::now();
        let shot_at = duration.mul_f64(0.6).min(Duration::from_secs(4));
        let (mut width, mut height) = (size.width, size.height);
        let mut shot_taken = shot.is_none();
        let mut closed_by_user = false;
        loop {
            while let Some(event) = conn.poll_for_event().map_err(s)? {
                match event {
                    Event::ConfigureNotify(e) if e.window == window => {
                        let new = (e.width as u32, e.height as u32);
                        if new != (width, height) {
                            (width, height) = new;
                            if can_resize {
                                let handle = plugin.instance.plugin_handle();
                                let wanted = GuiSize { width, height };
                                let adjusted = gui.adjust_size(&handle, wanted).unwrap_or(wanted);
                                let _ = gui.set_size(&handle, adjusted);
                            }
                        }
                    }
                    Event::ClientMessage(e) if e.data.as_data32()[0] == wm_delete => {
                        closed_by_user = true;
                    }
                    _ => {}
                }
            }
            let request = plugin.shared(|sh| sh.resize_requested.swap(0, Relaxed));
            if request != 0 {
                let wanted = GuiSize::unpack_from_u64(request);
                println!(
                    "gui: plugin requested resize to {}x{}",
                    wanted.width, wanted.height
                );
                conn.configure_window(
                    window,
                    &ConfigureWindowAux::new()
                        .width(wanted.width)
                        .height(wanted.height),
                )
                .map_err(s)?;
                conn.flush().map_err(s)?;
            }
            plugin.pump();

            let elapsed = started.elapsed();
            if !shot_taken && elapsed >= shot_at {
                shot_taken = true;
                match capture(conn, window, shot.unwrap()) {
                    Ok(report) => println!("gui: {report}"),
                    Err(e) => println!("gui: screenshot failed: {e}"),
                }
            }
            if closed_by_user || elapsed >= duration || plugin.shared(|sh| sh.gui_closed.load(Relaxed)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }

        let (timers, fds) = plugin
            .instance
            .access_handler(|m| (m.timers_registered.get(), m.fds_registered.get()));
        println!(
            "gui: ran {:.1} s, final host window {width}x{height}, closed by user: {closed_by_user}, plugin registered {timers} timer(s) and {fds} fd(s)",
            started.elapsed().as_secs_f64()
        );
        let _ = gui.hide(&plugin.instance.plugin_handle());
        conn.unmap_window(window).map_err(s)?;
        conn.flush().map_err(s)?;
        Ok(())
    }

    /// Grabs the window (including the plugin's child windows) and writes a PNG.
    fn capture(conn: &RustConnection, window: u32, path: &Path) -> Result<String, String> {
        let tree = conn.query_tree(window).map_err(s)?.reply().map_err(s)?;
        let mut children = Vec::new();
        for &child in &tree.children {
            let g = conn.get_geometry(child).map_err(s)?.reply().map_err(s)?;
            children.push(format!("{}x{}+{}+{}", g.width, g.height, g.x, g.y));
        }
        let geometry = conn.get_geometry(window).map_err(s)?.reply().map_err(s)?;
        let (w, h) = (geometry.width, geometry.height);
        let image = conn
            .get_image(ImageFormat::Z_PIXMAP, window, 0, 0, w, h, !0)
            .map_err(s)?
            .reply()
            .map_err(s)?;
        let pixels = w as usize * h as usize;
        if image.data.len() < pixels * 4 {
            return Err(format!(
                "unexpected image format: depth {}, {} bytes",
                image.depth,
                image.data.len()
            ));
        }
        let mut rgb = Vec::with_capacity(pixels * 3);
        let mut colours = HashSet::new();
        let mut lit = 0usize;
        for px in image.data.as_chunks::<4>().0.iter().take(pixels) {
            let (b, g, r) = (px[0], px[1], px[2]);
            rgb.extend_from_slice(&[r, g, b]);
            if r as u32 + g as u32 + b as u32 > 24 {
                lit += 1;
            }
            if colours.len() < 100_000 {
                colours.insert([r, g, b]);
            }
        }
        let file = std::fs::File::create(path).map_err(s)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .map_err(s)?
            .write_image_data(&rgb)
            .map_err(s)?;
        Ok(format!(
            "screenshot {}x{} -> {}, plugin child windows [{}], {:.0}% non-black pixels, {} distinct colours",
            w,
            h,
            path.display(),
            children.join(", "),
            100.0 * lit as f64 / pixels.max(1) as f64,
            colours.len()
        ))
    }

    fn floating(plugin: &mut Plugin, gui: PluginGui, duration: Duration) -> Result<(), String> {
        let handle = plugin.instance.plugin_handle();
        let config = [GuiApiType::WAYLAND, GuiApiType::X11]
            .into_iter()
            .map(|api_type| GuiConfiguration {
                api_type,
                is_floating: true,
            })
            .find(|&c| gui.is_api_supported(&handle, c))
            .ok_or("plugin supports no floating window API on this platform")?;
        println!("gui: floating window via {:?}", config.api_type);
        gui.create(&handle, config).map_err(s)?;
        gui.suggest_title(&handle, c"mdaw spike: plugin window");
        let shown = gui.show(&handle).map_err(|e| format!("show: {e}"));
        if shown.is_ok() {
            let started = Instant::now();
            while started.elapsed() < duration && !plugin.shared(|sh| sh.gui_closed.load(Relaxed)) {
                plugin.pump();
                std::thread::sleep(Duration::from_millis(4));
            }
            let (timers, fds) = plugin
                .instance
                .access_handler(|m| (m.timers_registered.get(), m.fds_registered.get()));
            println!(
                "gui: floating window ran {:.1} s, plugin registered {timers} timer(s) and {fds} fd(s)",
                started.elapsed().as_secs_f64()
            );
            let _ = gui.hide(&plugin.instance.plugin_handle());
        }
        gui.destroy(&plugin.instance.plugin_handle());
        plugin.pump();
        shown
    }
}
