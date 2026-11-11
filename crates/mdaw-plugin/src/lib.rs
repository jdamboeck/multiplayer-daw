//! Plugin hosting behind an interface that knows no plugin format.
//!
//! The rest of the program sees [`Host`], [`PluginInfo`] and [`Instance`], and for the audio
//! thread `mdaw_dsp::ExternalDevice`. Each format is a [`Backend`]: CLAP is the one there is
//! (`clap`). Another format, such as VST3, is another module implementing [`Backend`] and
//! [`Instance`], one more variant of `mdaw_model::PluginFormat`, and one line in
//! [`Host::standard`]; nothing else changes.
//!
//! Instances belong to the thread that made them, which plugins treat as their main thread
//! (that of the user interface). Their processors run on the audio thread.

mod clap;
#[cfg(target_os = "linux")]
pub mod isolated;

pub use mdaw_dsp::ExternalDevice;
pub use mdaw_model::PluginFormat;

use std::fmt;

/// One plugin that can be made.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginInfo {
    pub format: PluginFormat,
    /// Unique within its format; what the document stores.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// Makes sound from notes, as opposed to changing sound it is given.
    pub instrument: bool,
    /// Where it was found, for the user.
    pub location: String,
}

/// A parameter of an instance, with its value now.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    /// Where the plugin groups it, such as "Oscillators/1"; may be empty.
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    /// Takes whole numbers only.
    pub stepped: bool,
}

/// How an instance is going to be run.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub sample_rate: u32,
    /// Most frames one `process` call gets.
    pub max_block: usize,
    /// It renders faster than real time, on this thread, as for a bounce. Plugins that can
    /// switch to a better quality for that do.
    pub offline: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PluginError(pub String);

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PluginError {}

pub(crate) fn error(what: impl fmt::Display) -> PluginError {
    PluginError(what.to_string())
}

/// One plugin format.
pub trait Backend {
    fn format(&self) -> PluginFormat;
    /// Looks for plugins. May load and run plugin code.
    fn scan(&mut self) -> Vec<PluginInfo>;
    /// Makes a running instance of the plugin `id`, found by the last scan.
    fn create(&mut self, id: &str, settings: Settings) -> Result<Box<dyn Instance>, PluginError>;
}

/// A running plugin, on the thread that made it.
pub trait Instance {
    fn info(&self) -> &PluginInfo;
    /// The part that runs on the audio thread. There is one; `None` once it has been taken.
    fn take_processor(&mut self) -> Option<Box<dyn ExternalDevice>>;
    /// Whether the processor is out and has not come back yet: then the instance must be kept
    /// until it has (see [`Host::dispose`]).
    fn busy(&self) -> bool;
    fn save_state(&mut self) -> Result<Vec<u8>, PluginError>;
    fn load_state(&mut self, bytes: &[u8]) -> Result<(), PluginError>;
    /// The first `limit` parameters, with their values now. Asking for a few of a plugin with
    /// hundreds is cheap; their descriptions are kept.
    fn params(&mut self, limit: usize) -> Vec<ParamInfo>;
    fn param_count(&self) -> usize;
    /// Sets a parameter, as a user turning a knob would.
    fn set_param(&mut self, id: u32, value: f64);
    /// Its state changed since it was last saved or loaded: from its editor, or a parameter set
    /// from outside.
    fn changed(&self) -> bool;
    fn has_editor(&self) -> bool;
    /// Opens the plugin's own window.
    fn open_editor(&mut self, title: &str) -> Result<(), PluginError>;
    fn close_editor(&mut self);
    fn editor_open(&self) -> bool;
    /// The plugin crashed or stopped answering. It makes no sound from then on; make a new
    /// instance to hear it again.
    fn crashed(&self) -> bool {
        false
    }
    /// What a plugin expects of the main loop: its timers, the callbacks it asked for, its
    /// window's events, taking back its processor. Call often, at least once per UI frame.
    fn idle(&mut self);
}

/// All formats, and the plugins they found.
pub struct Host {
    backends: Vec<Box<dyn Backend>>,
    catalog: Vec<PluginInfo>,
    /// Instances let go of while their processor was still out.
    disposed: Vec<Box<dyn Instance>>,
}

impl Host {
    pub fn new(backends: Vec<Box<dyn Backend>>) -> Self {
        let mut host = Self {
            backends,
            catalog: Vec::new(),
            disposed: Vec::new(),
        };
        host.rescan();
        host
    }

    /// Every format this build hosts, with the plugins installed in the usual places, and with
    /// `test_synth` the built-in test synth, which can be told to crash. Where it can, each
    /// plugin runs in a helper process of its own, so that a crash takes only that plugin down;
    /// `MDAW_PLUGINS_IN_PROCESS=1` runs them inside.
    pub fn standard(test_synth: bool) -> Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("MDAW_PLUGINS_IN_PROCESS").is_none() {
            match isolated::helper() {
                Some(helper) => {
                    let backend = isolated::IsolatedBackend::standard(helper, test_synth);
                    return Self::new(vec![Box::new(backend)]);
                }
                None => eprintln!(
                    "mdaw: {} not found; plugins run inside mdaw, and a plugin crash ends it",
                    isolated::HELPER
                ),
            }
        }
        Self::new(vec![Box::new(clap::ClapBackend::standard(test_synth))])
    }

    /// The built-in test synth in a helper process: for tests of crash isolation.
    #[cfg(target_os = "linux")]
    pub fn isolated_test_synth(helper: std::path::PathBuf) -> Self {
        Self::new(vec![Box::new(isolated::IsolatedBackend::test_synth_only(helper))])
    }

    /// Only the built-in test synth, whatever is installed: for tests.
    pub fn test_synth_only() -> Self {
        Self::new(vec![Box::new(clap::ClapBackend::test_synth_only())])
    }

    pub fn rescan(&mut self) {
        self.catalog = self.backends.iter_mut().flat_map(|b| b.scan()).collect();
        self.catalog
            .sort_by(|a, b| (a.name.to_lowercase(), &a.id).cmp(&(b.name.to_lowercase(), &b.id)));
    }

    pub fn catalog(&self) -> &[PluginInfo] {
        &self.catalog
    }

    pub fn find(&self, format: PluginFormat, id: &str) -> Option<&PluginInfo> {
        self.catalog.iter().find(|p| p.format == format && p.id == id)
    }

    pub fn create(
        &mut self,
        format: PluginFormat,
        id: &str,
        settings: Settings,
    ) -> Result<Box<dyn Instance>, PluginError> {
        if self.find(format, id).is_none() {
            return Err(PluginError(format!(
                "{} plugin {id} is not installed",
                format.name()
            )));
        }
        let backend = self
            .backends
            .iter_mut()
            .find(|b| b.format() == format)
            .ok_or_else(|| error(format!("no {} host", format.name())))?;
        backend.create(id, settings)
    }

    /// Lets go of an instance. One whose processor is still out is kept until it is back.
    pub fn dispose(&mut self, mut instance: Box<dyn Instance>) {
        instance.close_editor();
        if instance.busy() {
            self.disposed.push(instance);
        }
    }

    /// Lets the instances let go of finish. Call once per UI frame.
    pub fn idle(&mut self) {
        for instance in &mut self.disposed {
            instance.idle();
        }
        self.disposed.retain(|i| i.busy());
    }
}

/// The test synth's id, for tests.
pub const TEST_SYNTH: &str = mdaw_test_plugin::ID;
