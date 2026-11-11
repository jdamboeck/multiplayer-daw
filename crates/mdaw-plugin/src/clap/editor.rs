//! A plugin's own window. Plugins draw into a window the host makes for them (on Linux an X11
//! window, through XWayland on a Wayland desktop; on Windows a Win32 window), or, if they can,
//! into one of their own ("floating").

use super::handlers::MdawHost;
use crate::{PluginError, error};
use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui};
use clack_host::prelude::*;
use std::ffi::CString;
use std::sync::atomic::Ordering::Relaxed;

/// A plugin's window. Closing it only hides it: tearing a plugin's interface down is where
/// plugins crash, at an awkward moment (a menu of theirs still open, say). It is destroyed once,
/// with the instance, after it has been hidden for a while.
pub struct Editor {
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    window: Option<native::Window>,
    hidden: bool,
}

fn floating_api() -> GuiApiType<'static> {
    if cfg!(windows) {
        GuiApiType::WIN32
    } else if cfg!(target_os = "macos") {
        GuiApiType::COCOA
    } else {
        GuiApiType::X11
    }
}

impl Editor {
    pub fn open(
        instance: &mut PluginInstance<MdawHost>,
        gui: PluginGui,
        title: &str,
    ) -> Result<Self, PluginError> {
        let handle = instance.plugin_handle();
        if let Some(api) = native::API {
            let embedded = GuiConfiguration {
                api_type: api,
                is_floating: false,
            };
            if gui.is_api_supported(&handle, embedded) {
                gui.create(&handle, embedded).map_err(error)?;
                match native::Window::open(&handle, gui, title) {
                    Ok(window) => {
                        // Some plugins say showing failed and show anyway.
                        let _ = gui.show(&handle);
                        return Ok(Self {
                            window: Some(window),
                            hidden: false,
                        });
                    }
                    Err(e) => {
                        gui.destroy(&handle);
                        if !gui.is_api_supported(
                            &handle,
                            GuiConfiguration {
                                api_type: floating_api(),
                                is_floating: true,
                            },
                        ) {
                            return Err(e);
                        }
                    }
                }
            }
        }
        let floating = GuiConfiguration {
            api_type: floating_api(),
            is_floating: true,
        };
        if !gui.is_api_supported(&handle, floating) {
            return Err(error("the plugin's window cannot be shown on this system"));
        }
        gui.create(&handle, floating).map_err(error)?;
        if let Ok(title) = CString::new(title) {
            gui.suggest_title(&handle, &title);
        }
        if let Err(e) = gui.show(&handle) {
            gui.destroy(&handle);
            return Err(error(format!("showing the plugin's window: {e}")));
        }
        Ok(Self {
            window: None,
            hidden: false,
        })
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// Takes the window off the screen, keeping the plugin's interface.
    pub fn hide(&mut self, instance: &mut PluginInstance<MdawHost>, gui: PluginGui) {
        if self.hidden {
            return;
        }
        self.hidden = true;
        let _ = gui.hide(&instance.plugin_handle());
        if let Some(window) = self.window.as_mut() {
            window.hide();
        }
    }

    /// Puts a hidden window back.
    pub fn show(&mut self, instance: &mut PluginInstance<MdawHost>, gui: PluginGui) {
        if !self.hidden {
            return;
        }
        self.hidden = false;
        if let Some(window) = self.window.as_mut() {
            window.show();
        }
        let _ = gui.show(&instance.plugin_handle());
    }

    /// Handles the window's events. False once the user closed it.
    pub fn poll(&mut self, instance: &mut PluginInstance<MdawHost>, gui: PluginGui) -> bool {
        if self.hidden {
            return true;
        }
        let request = instance.access_shared_handler(|s| s.resize_requested.swap(0, Relaxed));
        let Some(window) = self.window.as_mut() else {
            return true;
        };
        if request != 0 {
            window.resize(GuiSize::unpack_from_u64(request));
        }
        let handle = instance.plugin_handle();
        window.poll(&handle, gui)
    }

    /// Destroys the plugin's interface and the window. Hide it first, and give the plugin a
    /// moment.
    pub fn close(mut self, instance: &mut PluginInstance<MdawHost>, gui: PluginGui) {
        self.hide(instance, gui);
        let handle = instance.plugin_handle();
        gui.destroy(&handle);
        // The host's window goes after the plugin has let go of it.
        drop(self.window);
    }
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use clack_extensions::gui::Window as GuiWindow;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    pub const API: Option<GuiApiType<'static>> = Some(GuiApiType::X11);

    /// A top-level X11 window the plugin's editor lives in.
    pub struct Window {
        conn: RustConnection,
        id: u32,
        wm_delete: u32,
        size: (u32, u32),
        resizable: bool,
    }

    fn s(e: impl std::fmt::Display) -> PluginError {
        error(e)
    }

    impl Window {
        pub fn open(
            handle: &PluginMainThreadHandle,
            gui: PluginGui,
            title: &str,
        ) -> Result<Self, PluginError> {
            let (conn, screen) = x11rb::connect(None).map_err(|e| error(format!("no X11 display: {e}")))?;
            let root = conn.setup().roots[screen].root;
            let black = conn.setup().roots[screen].black_pixel;
            let size = gui.get_size(handle).unwrap_or(GuiSize {
                width: 800,
                height: 600,
            });
            let resizable = gui.can_resize(handle);
            let id = conn.generate_id().map_err(s)?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                root,
                0,
                0,
                size.width.clamp(1, 8192) as u16,
                size.height.clamp(1, 8192) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(black)
                    .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::EXPOSURE),
            )
            .map_err(s)?;
            conn.change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                title.as_bytes(),
            )
            .map_err(s)?;
            if !resizable {
                // A fixed size, so that tiling window managers float the window instead of
                // stretching it.
                let fixed = Some((size.width as i32, size.height as i32));
                x11rb::properties::WmSizeHints {
                    min_size: fixed,
                    max_size: fixed,
                    ..Default::default()
                }
                .set_normal_hints(&conn, id)
                .map_err(s)?;
            }
            let atom = |name: &[u8]| -> Result<u32, PluginError> {
                Ok(conn.intern_atom(false, name).map_err(s)?.reply().map_err(s)?.atom)
            };
            let protocols = atom(b"WM_PROTOCOLS")?;
            let wm_delete = atom(b"WM_DELETE_WINDOW")?;
            conn.change_property32(PropMode::REPLACE, id, protocols, AtomEnum::ATOM, &[wm_delete])
                .map_err(s)?;
            conn.flush().map_err(s)?;
            // SAFETY: the window lives until after the plugin destroyed its editor: `close`
            // destroys the editor before dropping this.
            unsafe { gui.set_parent(handle, GuiWindow::from_x11_handle(id as std::ffi::c_ulong)) }
                .map_err(|e| error(format!("embedding the plugin's window: {e}")))?;
            conn.map_window(id).map_err(s)?;
            conn.flush().map_err(s)?;
            Ok(Self {
                conn,
                id,
                wm_delete,
                size: (size.width, size.height),
                resizable,
            })
        }

        pub fn hide(&mut self) {
            let _ = self.conn.unmap_window(self.id);
            let _ = self.conn.flush();
        }

        pub fn show(&mut self) {
            let _ = self.conn.map_window(self.id);
            let _ = self.conn.flush();
        }

        pub fn resize(&mut self, size: GuiSize) {
            let aux = ConfigureWindowAux::new().width(size.width).height(size.height);
            let _ = self.conn.configure_window(self.id, &aux);
            let _ = self.conn.flush();
        }

        pub fn poll(&mut self, handle: &PluginMainThreadHandle, gui: PluginGui) -> bool {
            loop {
                let event = match self.conn.poll_for_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => return true,
                    Err(_) => return false,
                };
                match event {
                    Event::ConfigureNotify(e) if e.window == self.id => {
                        let new = (e.width as u32, e.height as u32);
                        if new != self.size {
                            self.size = new;
                            if self.resizable {
                                let wanted = GuiSize {
                                    width: new.0,
                                    height: new.1,
                                };
                                let adjusted = gui.adjust_size(handle, wanted).unwrap_or(wanted);
                                let _ = gui.set_size(handle, adjusted);
                            }
                        }
                    }
                    Event::ClientMessage(e) if e.data.as_data32()[0] == self.wm_delete => return false,
                    _ => {}
                }
            }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            let _ = self.conn.destroy_window(self.id);
            let _ = self.conn.flush();
        }
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use clack_extensions::gui::Window as GuiWindow;
    use std::sync::Once;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub const API: Option<GuiApiType<'static>> = Some(GuiApiType::WIN32);

    const CLASS: &[u16] = &[
        b'm' as u16,
        b'd' as u16,
        b'a' as u16,
        b'w' as u16,
        b'-' as u16,
        b'p' as u16,
        b'l' as u16,
        b'u' as u16,
        b'g' as u16,
        b'i' as u16,
        b'n' as u16,
        0,
    ];

    /// A top-level Win32 window the plugin's editor lives in. Its messages are dispatched by the
    /// message loop of the UI, which runs on the same thread.
    pub struct Window {
        hwnd: HWND,
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_CLOSE {
            // Hidden, not destroyed: `poll` sees that and the host closes the editor properly.
            // SAFETY: `hwnd` is the window this procedure was called for.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            return 0;
        }
        // SAFETY: passing on a message to the default procedure, as Windows expects.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn register() {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            // SAFETY: a plain window class with a valid procedure and a static name.
            unsafe {
                let class = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(window_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: GetModuleHandleW(std::ptr::null()),
                    hIcon: std::ptr::null_mut(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: CLASS.as_ptr(),
                };
                RegisterClassW(&class);
            }
        });
    }

    impl Window {
        pub fn open(
            handle: &PluginMainThreadHandle,
            gui: PluginGui,
            title: &str,
        ) -> Result<Self, PluginError> {
            register();
            let size = gui.get_size(handle).unwrap_or(GuiSize {
                width: 800,
                height: 600,
            });
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: size.width as i32,
                bottom: size.height as i32,
            };
            let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
            // SAFETY: plain Win32 calls with valid arguments; the window is destroyed in `drop`.
            let hwnd = unsafe {
                AdjustWindowRect(&mut rect, style, 0);
                CreateWindowExW(
                    0,
                    CLASS.as_ptr(),
                    title.as_ptr(),
                    style,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err(error("could not make a window for the plugin"));
            }
            // SAFETY: the window lives until after the plugin destroyed its editor.
            if let Err(e) = unsafe { gui.set_parent(handle, GuiWindow::from_win32_hwnd(hwnd)) } {
                // SAFETY: the window made just above.
                unsafe { DestroyWindow(hwnd) };
                return Err(error(format!("embedding the plugin's window: {e}")));
            }
            // SAFETY: a live window.
            unsafe { ShowWindow(hwnd, SW_SHOW) };
            Ok(Self { hwnd })
        }

        pub fn hide(&mut self) {
            // SAFETY: a live window.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }

        pub fn show(&mut self) {
            // SAFETY: a live window.
            unsafe { ShowWindow(self.hwnd, SW_SHOW) };
        }

        pub fn resize(&mut self, size: GuiSize) {
            // SAFETY: a live window.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    size.width as i32,
                    size.height as i32,
                    SWP_NOMOVE | SWP_NOZORDER,
                );
            }
        }

        pub fn poll(&mut self, _handle: &PluginMainThreadHandle, _gui: PluginGui) -> bool {
            // SAFETY: a live window.
            unsafe { IsWindowVisible(self.hwnd) != 0 }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the window this made, not yet destroyed.
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod native {
    use super::*;

    pub const API: Option<GuiApiType<'static>> = None;

    pub struct Window;

    impl Window {
        pub fn open(_: &PluginMainThreadHandle, _: PluginGui, _: &str) -> Result<Self, PluginError> {
            Err(error("embedded plugin windows are not supported here"))
        }

        pub fn hide(&mut self) {}

        pub fn show(&mut self) {}

        pub fn resize(&mut self, _: GuiSize) {}

        pub fn poll(&mut self, _: &mut PluginMainThreadHandle, _: PluginGui) -> bool {
            true
        }
    }
}
