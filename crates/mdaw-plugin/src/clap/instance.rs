//! The main-thread half of a CLAP instance, behind [`crate::Instance`].

use super::editor::Editor;
use super::handlers::{self, Exts, MdawHost};
use super::processor::{ClapProcessor, ParamChange, Ports};
use crate::{ExternalDevice, Instance, ParamInfo, PluginError, PluginInfo, Settings, error};
use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer};
use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags};
use clack_extensions::render::RenderMode;
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

/// Parameter changes that may wait for the audio thread at once.
const PARAM_CHANGES: usize = 1024;

pub struct ClapInstance {
    info: PluginInfo,
    instance: PluginInstance<MdawHost>,
    exts: Exts,
    /// The parameters' descriptions, as last read, with their cookies for the events that change
    /// them. Values are read when asked for.
    params: Vec<(ParamInfo, Cookie)>,
    params_to_audio: rtrb::Producer<ParamChange>,
    /// Counts the readings of the plugin's parameters, shared with the processor.
    generation: Arc<AtomicU32>,
    back: Receiver<StoppedPluginAudioProcessor<MdawHost>>,
    /// The processor, until it is taken.
    processor: Option<Box<ClapProcessor>>,
    /// The processor is out and not back yet.
    out: bool,
    editor: Option<Editor>,
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl ClapInstance {
    /// Makes and activates an instance. Call on the thread that is to be its main thread.
    pub fn new(entry: &PluginEntry, info: PluginInfo, settings: Settings) -> Result<Self, PluginError> {
        let host = HostInfo::new(
            "mdaw",
            "mdaw",
            "https://github.com/jdamboeck/multiplayer-daw",
            "0.1.0",
        )
        .map_err(error)?;
        let id = CString::new(info.id.as_str()).map_err(error)?;
        let (shared, main) = handlers::handlers();
        let mut instance = PluginInstance::<MdawHost>::new(shared, main, entry, &id, &host)
            .map_err(|e| error(format!("starting {}: {e}", info.name)))?;
        let exts = instance
            .access_handler(|m| m.exts.get().copied())
            .unwrap_or_default();
        if settings.offline {
            instance.access_shared_handler(|s| s.offline.store(true, Relaxed));
            if let Some(render) = exts.render {
                let _ = render.set(&instance.plugin_handle(), RenderMode::Offline);
            }
        }
        let ports = |instance: &mut PluginInstance<MdawHost>, is_input: bool| -> (Vec<u32>, Option<usize>) {
            let Some(ext) = exts.audio_ports else {
                return (Vec::new(), None);
            };
            let handle = instance.plugin_handle();
            let mut buffer = AudioPortInfoBuffer::new();
            let mut channels = Vec::new();
            let mut main = None;
            for i in 0..ext.count(&handle, is_input) {
                if let Some(port) = ext.get(&handle, i, is_input, &mut buffer) {
                    if port.flags.contains(AudioPortFlags::IS_MAIN) && main.is_none() {
                        main = Some(channels.len());
                    }
                    channels.push(port.channel_count);
                }
            }
            // Without a port marked main, the first is.
            let main = main.or((!channels.is_empty()).then_some(0));
            (channels, main)
        };
        let (in_channels, in_main) = ports(&mut instance, true);
        let (out_channels, out_main) = ports(&mut instance, false);
        let stopped = instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: settings.sample_rate as f64,
                    min_frames_count: 1,
                    max_frames_count: settings.max_block as u32,
                },
            )
            .map_err(|e| error(format!("activating {}: {e}", info.name)))?;
        let (to_audio, from_main) = rtrb::RingBuffer::new(PARAM_CHANGES);
        let (back_tx, back) = channel();
        let generation = Arc::new(AtomicU32::new(0));
        let processor = ClapProcessor::new(
            stopped.into(),
            back_tx,
            Ports::new(&in_channels, in_main, settings.max_block),
            Ports::new(&out_channels, out_main, settings.max_block),
            from_main,
            generation.clone(),
            settings.max_block,
        );
        let mut made = Self {
            info,
            instance,
            exts,
            params: Vec::new(),
            generation,
            params_to_audio: to_audio,
            back,
            processor: Some(Box::new(processor)),
            out: false,
            editor: None,
        };
        made.params = made.param_infos();
        Ok(made)
    }

    fn param_infos(&mut self) -> Vec<(ParamInfo, Cookie)> {
        let Some(ext) = self.exts.params else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let mut buffer = ParamInfoBuffer::new();
        (0..ext.count(&handle))
            .filter_map(|i| {
                let info = ext.get_info(&handle, i, &mut buffer)?;
                if info.flags.contains(ParamInfoFlags::IS_HIDDEN)
                    || info.flags.contains(ParamInfoFlags::IS_READONLY)
                {
                    return None;
                }
                let value = ext.get_value(&handle, info.id).unwrap_or(info.default_value);
                Some((
                    ParamInfo {
                        id: info.id.get(),
                        name: text(info.name),
                        module: text(info.module),
                        min: info.min_value,
                        max: info.max_value,
                        default: info.default_value,
                        value,
                        stepped: info.flags.contains(ParamInfoFlags::IS_STEPPED),
                    },
                    info.cookie,
                ))
            })
            .collect()
    }

    fn set_changed(&self, changed: bool) {
        self.instance
            .access_shared_handler(|s| s.changed.store(changed, Relaxed));
    }

    /// Runs what the plugin expects from the main loop: callbacks it asked for, timers and
    /// file descriptors.
    fn pump(&mut self) {
        if self
            .instance
            .access_shared_handler(|s| s.callback_requested.swap(false, Relaxed))
        {
            self.instance.call_on_main_thread_callback();
        }
        if let Some(timer) = self.exts.timer {
            let due = self.instance.access_handler(|m| m.due_timers(Instant::now()));
            for id in due {
                timer.on_timer(&self.instance.plugin_handle(), id);
            }
        }
        #[cfg(unix)]
        self.pump_fds();
    }

    #[cfg(unix)]
    fn pump_fds(&mut self) {
        use clack_extensions::posix_fd::FdFlags;
        let Some(ext) = self.exts.posix_fd else {
            return;
        };
        let fds = self.instance.access_handler(|m| m.fds.borrow().clone());
        if fds.is_empty() {
            return;
        }
        let mut poll: Vec<libc::pollfd> = fds
            .iter()
            .map(|&(fd, flags)| {
                let mut events = 0;
                if flags.contains(FdFlags::READ) {
                    events |= libc::POLLIN;
                }
                if flags.contains(FdFlags::WRITE) {
                    events |= libc::POLLOUT;
                }
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                }
            })
            .collect();
        // SAFETY: `poll` points at `poll.len()` valid pollfd structs; zero timeout.
        let ready = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, 0) };
        if ready <= 0 {
            return;
        }
        for p in poll.iter().filter(|p| p.revents != 0) {
            let mut flags = FdFlags::empty();
            if p.revents & libc::POLLIN != 0 {
                flags |= FdFlags::READ;
            }
            if p.revents & libc::POLLOUT != 0 {
                flags |= FdFlags::WRITE;
            }
            if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                flags |= FdFlags::ERROR;
            }
            ext.on_fd(&self.instance.plugin_handle(), p.fd, flags);
        }
    }
}

impl Instance for ClapInstance {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn take_processor(&mut self) -> Option<Box<dyn ExternalDevice>> {
        let processor = self.processor.take()?;
        self.out = true;
        Some(processor)
    }

    fn busy(&self) -> bool {
        self.out
    }

    fn save_state(&mut self) -> Result<Vec<u8>, PluginError> {
        let ext = self
            .exts
            .state
            .ok_or_else(|| error(format!("{} cannot save its state", self.info.name)))?;
        let mut out = Vec::new();
        ext.save(&self.instance.plugin_handle(), &mut out)
            .map_err(|e| error(format!("saving the state of {}: {e}", self.info.name)))?;
        self.set_changed(false);
        Ok(out)
    }

    fn load_state(&mut self, mut bytes: &[u8]) -> Result<(), PluginError> {
        let ext = self
            .exts
            .state
            .ok_or_else(|| error(format!("{} cannot load a state", self.info.name)))?;
        ext.load(&self.instance.plugin_handle(), &mut bytes)
            .map_err(|e| error(format!("loading the state of {}: {e}", self.info.name)))?;
        self.set_changed(false);
        Ok(())
    }

    fn params(&mut self, limit: usize) -> Vec<ParamInfo> {
        let Some(ext) = self.exts.params else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        self.params
            .iter()
            .take(limit)
            .map(|(info, _)| {
                let value = ext.get_value(&handle, ClapId::new(info.id)).unwrap_or(info.value);
                ParamInfo {
                    value,
                    ..info.clone()
                }
            })
            .collect()
    }

    fn param_count(&self) -> usize {
        self.params.len()
    }

    fn set_param(&mut self, id: u32, value: f64) {
        let Some(&(_, cookie)) = self.params.iter().find(|(p, _)| p.id == id) else {
            return;
        };
        let id = ClapId::new(id);
        let generation = self.generation.load(Relaxed);
        let change = ParamChange {
            id,
            cookie,
            value,
            generation,
        };
        if self.params_to_audio.push(change).is_ok() {
            self.set_changed(true);
        }
    }

    fn changed(&self) -> bool {
        self.instance.access_shared_handler(|s| s.changed.load(Relaxed))
    }

    fn has_editor(&self) -> bool {
        self.exts.gui.is_some()
    }

    fn open_editor(&mut self, title: &str) -> Result<(), PluginError> {
        if let (Some(editor), Some(gui)) = (self.editor.as_mut(), self.exts.gui) {
            editor.show(&mut self.instance, gui);
            return Ok(());
        }
        let gui = self
            .exts
            .gui
            .ok_or_else(|| error(format!("{} has no window of its own", self.info.name)))?;
        self.instance
            .access_shared_handler(|s| s.gui_closed.store(false, Relaxed));
        self.editor = Some(Editor::open(&mut self.instance, gui, title)?);
        Ok(())
    }

    fn close_editor(&mut self) {
        if let (Some(editor), Some(gui)) = (self.editor.as_mut(), self.exts.gui) {
            editor.hide(&mut self.instance, gui);
        }
    }

    fn editor_open(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| !e.hidden())
    }

    fn crashed(&self) -> bool {
        self.instance.access_shared_handler(|s| s.failed.load(Relaxed))
    }

    fn idle(&mut self) {
        self.pump();
        if self.instance.access_handler(|m| m.params_changed.replace(false)) {
            // The cookies of changes still on their way are from the old reading.
            self.generation.fetch_add(1, Relaxed);
            self.params = self.param_infos();
        }
        if let (Some(editor), Some(gui)) = (self.editor.as_mut(), self.exts.gui) {
            let gone = self
                .instance
                .access_shared_handler(|s| s.gui_closed.swap(false, Relaxed));
            if gone {
                // The plugin closed its interface itself; CLAP wants it destroyed now.
                self.destroy_editor();
            } else if !editor.poll(&mut self.instance, gui) {
                editor.hide(&mut self.instance, gui);
            }
        }
        while let Ok(stopped) = self.back.try_recv() {
            self.instance.deactivate(stopped);
            self.out = false;
        }
    }
}

impl ClapInstance {
    /// Destroys the plugin's interface: hidden first, after the plugin had a few rounds of its
    /// main loop to settle.
    fn destroy_editor(&mut self) {
        let Some(gui) = self.exts.gui else {
            return;
        };
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        if !editor.hidden() {
            editor.hide(&mut self.instance, gui);
            for _ in 0..5 {
                self.pump();
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
        }
        editor.close(&mut self.instance, gui);
    }
}

impl Drop for ClapInstance {
    fn drop(&mut self) {
        self.destroy_editor();
        if let Some(processor) = self.processor.take() {
            drop(processor);
        }
        while let Ok(stopped) = self.back.try_recv() {
            self.instance.deactivate(stopped);
        }
    }
}
