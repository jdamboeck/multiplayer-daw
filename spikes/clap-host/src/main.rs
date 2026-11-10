//! Spike S1: can we host CLAP plugins from Rust with clack?
//!
//! Throwaway code. See `docs/spikes/clap-host.md` for what it found.

mod gui;
mod host;
mod scan;

use host::{Ev, Plugin};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: spike-clap-host scan
       spike-clap-host info   <plugin-id> [--params]
       spike-clap-host render <plugin-id> [--param <name-part>=<0..1>]... [--out <dir>]
       spike-clap-host play   <plugin-id> [--seconds <n>] [--gain <g>] [--gui [--shot <file.png>]]
       spike-clap-host gui    <plugin-id> [--seconds <n>] [--shot <file.png>] [--floating]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    let id = args.get(1).map(String::as_str);
    let result = match (args.first().map(String::as_str), id) {
        (Some("scan"), _) => scan_cmd(),
        (Some("info"), Some(id)) => info_cmd(id, flag("--params")),
        (Some("render"), Some(id)) => {
            let params: Vec<&str> = args
                .windows(2)
                .filter(|w| w[0] == "--param")
                .map(|w| w[1].as_str())
                .collect();
            render_cmd(id, &params, value("--out"))
        }
        (Some("play"), Some(id)) => play_cmd(
            id,
            value("--seconds").and_then(|s| s.parse().ok()).unwrap_or(3.0),
            value("--gain").and_then(|s| s.parse().ok()).unwrap_or(0.15),
            flag("--gui"),
            value("--shot").map(Path::new),
        ),
        (Some("gui"), Some(id)) => gui_cmd(
            id,
            value("--seconds").and_then(|s| s.parse().ok()).unwrap_or(6.0),
            value("--shot").map(Path::new),
            flag("--floating"),
        ),
        _ => Err(USAGE.to_string()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- scan / info

fn scan_cmd() -> Result<(), String> {
    println!("search paths: {:?}", scan::search_paths());
    let mut total = 0;
    for file in scan::plugin_files() {
        let started = Instant::now();
        match scan::describe(&file) {
            Ok((_entry, found)) => {
                println!("{} ({} ms)", file.display(), started.elapsed().as_millis());
                for f in found {
                    total += 1;
                    println!(
                        "  {:<40} {:<24} {:<12} {:<10} [{}]",
                        f.id,
                        f.name,
                        f.vendor,
                        f.version,
                        f.features.join(", ")
                    );
                }
            }
            Err(e) => println!("{}: FAILED: {e}", file.display()),
        }
    }
    println!("{total} plugins");
    Ok(())
}

fn info_cmd(id: &str, list_params: bool) -> Result<(), String> {
    let (entry, found) = scan::find(id)?;
    let mut plugin = Plugin::new(&entry, id)?;
    println!(
        "{} {} by {} ({})",
        found.name,
        found.version,
        found.vendor,
        found.file.display()
    );
    println!("features:   {}", found.features.join(", "));
    println!("extensions: {}", plugin.exts.names().join(", "));
    println!("audio in:   {:?}", plugin.audio_in);
    println!("audio out:  {:?}", plugin.audio_out);
    println!("note in:    {} port(s)", plugin.note_in);
    let params = plugin.params();
    println!("params:     {}", params.len());
    if list_params {
        for p in &params {
            println!(
                "  {:>10} {:<40} {:<24} [{}, {}] default {} value {:?} {:?}",
                p.id.get(),
                p.name,
                p.module,
                p.min,
                p.max,
                p.default,
                p.value,
                p.flags
            );
        }
    }
    gui::report_support(&mut plugin);
    Ok(())
}

// ---------------------------------------------------------------- offline render

const SR: f64 = 48_000.0;
const BLOCK: usize = 512;
const RENDER_SECONDS: f64 = 4.0;

/// A fixed little phrase: a chord, then a single higher note. Times in seconds.
fn note_script(sample_rate: f64) -> Vec<(u64, Ev)> {
    let at = |seconds: f64| (seconds * sample_rate) as u64;
    let mut script = vec![
        (
            at(0.05),
            Ev::On {
                key: 48,
                velocity: 0.8,
            },
        ),
        (
            at(0.05),
            Ev::On {
                key: 60,
                velocity: 0.8,
            },
        ),
        (
            at(0.05),
            Ev::On {
                key: 64,
                velocity: 0.8,
            },
        ),
        (
            at(0.05),
            Ev::On {
                key: 67,
                velocity: 0.8,
            },
        ),
        (at(1.50), Ev::Off { key: 48 }),
        (at(1.50), Ev::Off { key: 60 }),
        (at(1.50), Ev::Off { key: 64 }),
        (at(1.50), Ev::Off { key: 67 }),
        (
            at(2.00),
            Ev::On {
                key: 72,
                velocity: 0.9,
            },
        ),
        (at(2.50), Ev::Off { key: 72 }),
    ];
    script.sort_by_key(|e| e.0);
    script
}

/// Deterministic input for effects: gated saw plus sine.
fn test_signal(frame: u64, sample_rate: f64) -> f32 {
    let t = frame as f64 / sample_rate;
    let gate = if t % 1.0 < 0.6 { 1.0 } else { 0.0 };
    let saw = 2.0 * ((t * 110.0) % 1.0) - 1.0;
    (0.2 * gate * (0.6 * saw + 0.4 * (std::f64::consts::TAU * 440.0 * t).sin())) as f32
}

struct Audio {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl Audio {
    fn peak(&self) -> f32 {
        self.left
            .iter()
            .chain(&self.right)
            .fold(0.0, |m, s| m.max(s.abs()))
    }
    fn rms(&self) -> f64 {
        let sum: f64 = self
            .left
            .iter()
            .chain(&self.right)
            .map(|&s| (s as f64) * (s as f64))
            .sum();
        (sum / (self.left.len() + self.right.len()).max(1) as f64).sqrt()
    }
    /// FNV-1a over the raw sample bits; equal hashes mean bit-identical audio.
    fn hash(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for s in self.left.iter().chain(&self.right) {
            for b in s.to_bits().to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
        h
    }
    fn describe(&self) -> String {
        format!(
            "peak {:6.1} dBFS  rms {:6.1} dBFS  hash {:016x}  nan {}",
            db(self.peak() as f64),
            db(self.rms()),
            self.hash(),
            self.left
                .iter()
                .chain(&self.right)
                .filter(|s| !s.is_finite())
                .count()
        )
    }
    /// RMS per 50 ms window, both channels together.
    fn envelope(&self) -> Vec<f64> {
        let window = (SR * 0.05) as usize;
        self.left
            .chunks(window)
            .zip(self.right.chunks(window))
            .map(|(l, r)| {
                let sum: f64 = l.iter().chain(r).map(|&s| (s as f64) * (s as f64)).sum();
                (sum / (l.len() + r.len()) as f64).sqrt()
            })
            .collect()
    }
}

impl Audio {
    /// Energy per semitone (MIDI 24..108) over the first 1.5 s, by Goertzel.
    /// Ignores phase, so it can tell "same sound" from "same samples".
    fn pitch_profile(&self) -> Vec<f64> {
        let frames = self.left.len().min((SR * 1.5) as usize);
        (24..108)
            .map(|key| {
                let freq = 440.0 * 2f64.powf((key as f64 - 69.0) / 12.0);
                let coeff = 2.0 * (std::f64::consts::TAU * freq / SR).cos();
                let (mut s1, mut s2) = (0.0f64, 0.0f64);
                for i in 0..frames {
                    let x = (self.left[i] + self.right[i]) as f64;
                    let s0 = x + coeff * s1 - s2;
                    s2 = s1;
                    s1 = s0;
                }
                (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt()
            })
            .collect()
    }
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f64]| v.iter().map(|x| x * x).sum::<f64>().sqrt();
    dot / (norm(a) * norm(b)).max(1e-30)
}

fn db(x: f64) -> f64 {
    if x <= 0.0 { -999.0 } else { 20.0 * x.log10() }
}

fn compare(label: &str, a: &Audio, b: &Audio) {
    if a.hash() == b.hash() {
        println!("  {label}: BIT-IDENTICAL");
        return;
    }
    let diff = Audio {
        left: a.left.iter().zip(&b.left).map(|(x, y)| x - y).collect(),
        right: a.right.iter().zip(&b.right).map(|(x, y)| x - y).collect(),
    };
    let env_diff = a
        .envelope()
        .iter()
        .zip(b.envelope())
        .filter(|(x, y)| x.max(*y) > 1e-3)
        .map(|(x, y)| (db(*x) - db(y)).abs())
        .fold(0.0f64, f64::max);
    println!(
        "  {label}: differs. residual {:.1} dB rel. signal, worst 50 ms level difference {:.2} dB, \
         pitch-profile similarity {:.4}",
        db(diff.rms()) - db(a.rms().max(b.rms())),
        env_diff,
        cosine(&a.pitch_profile(), &b.pitch_profile())
    );
}

fn render(plugin: &mut Plugin, is_instrument: bool, extra: &[Ev]) -> Result<Audio, String> {
    let offline = plugin.set_offline(true);
    let mut processor = plugin.activate(SR, BLOCK as u32)?;
    plugin.shared(|s| s.offline_processing.store(true, Relaxed));
    let script = if is_instrument {
        note_script(SR)
    } else {
        Vec::new()
    };
    let total = (RENDER_SECONDS * SR) as u64;
    let mut out = Audio {
        left: Vec::new(),
        right: Vec::new(),
    };
    let (mut pos, mut next) = (0u64, 0usize);
    let mut events: Vec<(u32, Ev)> = Vec::new();
    let mut result = Ok(());
    while pos < total {
        let frames = BLOCK.min((total - pos) as usize);
        events.clear();
        if pos == 0 {
            events.extend(extra.iter().map(|&e| (0, e)));
        }
        while next < script.len() && script[next].0 < pos + frames as u64 {
            events.push(((script[next].0 - pos) as u32, script[next].1));
            next += 1;
        }
        processor.fill_main_in(frames, |i| test_signal(pos + i as u64, SR));
        if let Err(e) = processor.process(frames, &events) {
            result = Err(e);
            break;
        }
        let Some((l, r)) = processor.main_out(frames) else {
            result = Err("plugin has no audio output".into());
            break;
        };
        out.left.extend_from_slice(l);
        out.right.extend_from_slice(r);
        pos += frames as u64;
    }
    processor.stop();
    plugin.shared(|s| s.offline_processing.store(false, Relaxed));
    plugin.deactivate(processor);
    if offline.is_some() {
        plugin.set_offline(false);
    }
    plugin.pump();
    result.map(|()| out)
}

fn write_wav(path: &Path, audio: &Audio) -> std::io::Result<()> {
    let frames = audio.left.len();
    let data_len = (frames * 4) as u32;
    let mut bytes = Vec::with_capacity(44 + frames * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
    bytes.extend_from_slice(&(SR as u32).to_le_bytes());
    bytes.extend_from_slice(&(SR as u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for (l, r) in audio.left.iter().zip(&audio.right) {
        for s in [l, r] {
            bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    std::fs::write(path, bytes)
}

fn render_cmd(id: &str, param: &[&str], out_dir: Option<&str>) -> Result<(), String> {
    let (entry, found) = scan::find(id)?;
    let is_instrument = found.features.iter().any(|f| f == "instrument");
    println!(
        "{} {} ({})",
        found.name,
        found.version,
        if is_instrument { "instrument" } else { "effect" }
    );

    let started = Instant::now();
    let mut a = Plugin::new(&entry, id)?;
    println!("instantiate: {} ms", started.elapsed().as_millis());
    a.pump();

    // 1. Default state: twice on one instance, once on a fresh instance.
    let started = Instant::now();
    let a1 = render(&mut a, is_instrument, &[])?;
    let speed = RENDER_SECONDS / started.elapsed().as_secs_f64();
    println!("default render:  {}  ({speed:.0}x realtime)", a1.describe());
    let a2 = render(&mut a, is_instrument, &[])?;
    let mut fresh = Plugin::new(&entry, id)?;
    fresh.pump();
    let f1 = render(&mut fresh, is_instrument, &[])?;
    println!("determinism at default state:");
    compare("same instance, second run", &a1, &a2);
    compare("fresh instance             ", &a1, &f1);
    drop(fresh);

    // 2. Change one parameter so the saved state is not the default.
    let params = a.params();
    let wanted: Vec<(String, f64)> = if param.is_empty() {
        let fallback = ["cutoff", "drive", "gain", "volume", "level", "mix"];
        let hit = fallback
            .iter()
            .find_map(|w| params.iter().find(|p| p.name.to_lowercase().contains(w)))
            .or(params.first());
        hit.map(|p| (p.name.to_lowercase(), 0.25)).into_iter().collect()
    } else {
        let mut list = Vec::new();
        for spec in param {
            let (name, v) = spec.split_once('=').ok_or("--param wants <name-part>=<0..1>")?;
            list.push((name.to_lowercase(), v.parse::<f64>().map_err(|e| e.to_string())?));
        }
        list
    };
    let mut changed = Vec::new();
    for (name, norm) in &wanted {
        let Some(p) = params
            .iter()
            .find(|p| p.name.to_lowercase().contains(name) && p.max > p.min)
        else {
            return Err(format!("no parameter matches {name:?}"));
        };
        let value = p.min + norm * (p.max - p.min);
        println!(
            "param change: {:?} (id {}) {:?} -> {value}  range [{}, {}]",
            p.name,
            p.id.get(),
            p.value,
            p.min,
            p.max
        );
        // SPIKE_NULL_COOKIE=1 reproduces what a host that ignores cookies would send.
        let cookie = if std::env::var_os("SPIKE_NULL_COOKIE").is_some() {
            clack_host::utils::Cookie::empty()
        } else {
            p.cookie
        };
        changed.push((p.id, cookie, value));
    }
    let events: Vec<Ev> = changed
        .iter()
        .map(|&(id, cookie, value)| Ev::Param { id, cookie, value })
        .collect();
    let b1 = render(&mut a, is_instrument, &events)?;
    println!("changed render:  {}", b1.describe());
    compare("changed vs default (should differ)", &a1, &b1);
    let b1_again = render(&mut a, is_instrument, &[])?;
    compare("changed state, same instance again ", &b1, &b1_again);
    for &(id, _, value) in &changed {
        println!(
            "  instance A reports param = {:?} (set {value})",
            a.param_value(id)
        );
    }

    // 3. Save state, load it into a second instance, render offline there.
    let state = a.save_state()?;
    println!(
        "state: {} bytes, dirty flag raised by plugin: {}",
        state.len(),
        a.instance.access_handler(|m| m.state_dirty.get())
    );
    let mut b = Plugin::new(&entry, id)?;
    b.pump();
    b.load_state(&state)?;
    b.pump();
    for &(id, _, value) in &changed {
        println!(
            "  instance B reports param = {:?} after load (expected {value})",
            b.param_value(id)
        );
    }
    let b2 = render(&mut b, is_instrument, &[])?;
    println!("second instance: {}", b2.describe());
    compare("second instance with loaded state vs original", &b1, &b2);
    // Two instances that have done nothing but load the same state: the bounce recipe.
    let mut c = Plugin::new(&entry, id)?;
    c.pump();
    c.load_state(&state)?;
    c.pump();
    let c1 = render(&mut c, is_instrument, &[])?;
    compare("two fresh instances, same loaded state       ", &b2, &c1);
    let state2 = b.save_state()?;
    println!(
        "  state re-saved by second instance: {} bytes, {}",
        state2.len(),
        if state2 == state {
            "byte-identical".to_string()
        } else {
            let differing = state.iter().zip(&state2).filter(|(x, y)| x != y).count();
            format!("differs in {differing} byte(s)")
        }
    );
    let state3 = a.save_state()?;
    println!(
        "  state saved twice by the first instance: {}",
        if state3 == state {
            "byte-identical"
        } else {
            "differs"
        }
    );

    if let Some(dir) = out_dir {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stem = found.name.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        for (suffix, audio) in [("default", &a1), ("changed", &b1), ("second-instance", &b2)] {
            let path = dir.join(format!("{stem}-{suffix}.wav"));
            write_wav(&path, audio).map_err(|e| e.to_string())?;
            println!("wrote {}", path.display());
        }
        std::fs::write(dir.join(format!("{stem}.state")), &state).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------- realtime

fn play_cmd(id: &str, seconds: f64, gain: f32, with_gui: bool, shot: Option<&Path>) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let (entry, found) = scan::find(id)?;
    let is_instrument = found.features.iter().any(|f| f == "instrument");
    let mut plugin = Plugin::new(&entry, id)?;

    let host = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name().eq_ignore_ascii_case("pipewire"))
        .and_then(|h| cpal::host_from_id(h).ok())
        .unwrap_or_else(cpal::default_host);
    let device = host.default_output_device().ok_or("no default output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "spike only handles f32 output, device wants {}",
            supported.sample_format()
        ));
    }
    let config: cpal::StreamConfig = supported.into();
    let sample_rate = config.sample_rate as f64;
    let channels = config.channels as usize;
    println!(
        "audio host {:?}, {} Hz, {} channels, buffer {:?}",
        host.id().name(),
        config.sample_rate,
        channels,
        config.buffer_size
    );

    const MAX_FRAMES: usize = 4096;
    let processor = Arc::new(Mutex::new(plugin.activate(sample_rate, MAX_FRAMES as u32)?));
    let script = if is_instrument {
        note_script(sample_rate)
    } else {
        Vec::new()
    };

    struct Stats {
        callbacks: AtomicU64,
        frames: AtomicU64,
        max_block: AtomicU64,
        max_micros: AtomicU64,
        errors: AtomicU64,
        over_budget: AtomicU64,
        peak_bits: AtomicU32,
        stop: AtomicBool,
        stopped: AtomicBool,
    }
    let stats = Arc::new(Stats {
        callbacks: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        max_block: AtomicU64::new(0),
        max_micros: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        over_budget: AtomicU64::new(0),
        peak_bits: AtomicU32::new(0),
        stop: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
    });
    let stream_errors = Arc::new(AtomicU64::new(0));

    let stream = {
        let processor = processor.clone();
        let stats = stats.clone();
        let stream_errors = stream_errors.clone();
        let (mut pos, mut next) = (0u64, 0usize);
        let mut events: Vec<(u32, Ev)> = Vec::with_capacity(64);
        device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let started = Instant::now();
                    let mut processor = processor.lock().unwrap();
                    if stats.stop.load(Relaxed) {
                        // stop_processing() belongs to the audio thread, like process().
                        processor.stop();
                        data.fill(0.0);
                        stats.stopped.store(true, Relaxed);
                        return;
                    }
                    let total = data.len() / channels;
                    let mut done = 0;
                    let mut peak = 0f32;
                    while done < total {
                        let frames = (total - done).min(MAX_FRAMES);
                        events.clear();
                        while next < script.len() && script[next].0 < pos + frames as u64 {
                            events.push((script[next].0.saturating_sub(pos) as u32, script[next].1));
                            next += 1;
                        }
                        processor.fill_main_in(frames, |i| test_signal(pos + i as u64, sample_rate));
                        let ok = processor.process(frames, &events).is_ok();
                        let block = &mut data[done * channels..(done + frames) * channels];
                        block.fill(0.0);
                        match processor.main_out(frames) {
                            Some((l, r)) if ok => {
                                for (i, frame) in block.chunks_mut(channels).enumerate() {
                                    frame[0] = l[i] * gain;
                                    if channels > 1 {
                                        frame[1] = r[i] * gain;
                                    }
                                    peak = peak.max(l[i].abs()).max(r[i].abs());
                                }
                            }
                            _ => {
                                stats.errors.fetch_add(1, Relaxed);
                            }
                        }
                        pos += frames as u64;
                        done += frames;
                    }
                    stats.callbacks.fetch_add(1, Relaxed);
                    stats.frames.fetch_add(total as u64, Relaxed);
                    stats.max_block.fetch_max(total as u64, Relaxed);
                    let micros = started.elapsed().as_micros() as u64;
                    stats.max_micros.fetch_max(micros, Relaxed);
                    if micros as f64 > total as f64 / sample_rate * 1e6 {
                        stats.over_budget.fetch_add(1, Relaxed);
                    }
                    stats.peak_bits.fetch_max(peak.to_bits(), Relaxed);
                },
                move |err| {
                    stream_errors.fetch_add(1, Relaxed);
                    eprintln!("    [stream] {err}");
                },
                None,
            )
            .map_err(|e| e.to_string())?
    };
    stream.play().map_err(|e| e.to_string())?;
    println!("playing {seconds} s at gain {gain} ...");
    let gui_result = if with_gui {
        // The realistic case: editor open on the main thread while the audio thread processes.
        gui::run(&mut plugin, Duration::from_secs_f64(seconds), shot, false)
    } else {
        plugin.pump_for(Duration::from_secs_f64(seconds));
        Ok(())
    };
    stats.stop.store(true, Relaxed);
    let deadline = Instant::now() + Duration::from_millis(500);
    while !stats.stopped.load(Relaxed) && Instant::now() < deadline {
        plugin.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(stream);

    let callbacks = stats.callbacks.load(Relaxed);
    let frames = stats.frames.load(Relaxed);
    println!(
        "callbacks {callbacks}, frames {frames} ({:.2} s), avg block {:.0}, max block {}, \
         worst callback {} us, callbacks over their time budget {}, process errors {}, \
         stream errors {}, plugin peak {:.1} dBFS",
        frames as f64 / sample_rate,
        frames as f64 / callbacks.max(1) as f64,
        stats.max_block.load(Relaxed),
        stats.max_micros.load(Relaxed),
        stats.over_budget.load(Relaxed),
        stats.errors.load(Relaxed),
        stream_errors.load(Relaxed),
        db(f32::from_bits(stats.peak_bits.load(Relaxed)) as f64),
    );

    let mut processor = Arc::try_unwrap(processor)
        .map_err(|_| "audio callback still holds the processor")?
        .into_inner()
        .map_err(|e| e.to_string())?;
    processor.stop();
    plugin.deactivate(processor);
    gui_result
}

// ---------------------------------------------------------------- gui

fn gui_cmd(id: &str, seconds: f64, shot: Option<&Path>, floating: bool) -> Result<(), String> {
    let (entry, _found) = scan::find(id)?;
    let mut plugin = Plugin::new(&entry, id)?;
    gui::report_support(&mut plugin);
    gui::run(&mut plugin, Duration::from_secs_f64(seconds), shot, floating)
}
