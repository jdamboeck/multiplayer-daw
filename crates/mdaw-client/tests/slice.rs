//! The collaborative slice on Tracktion Engine, with real processes: a server in this process
//! and clients that are `mdaw-slice` processes (one engine per process), each with its own
//! profile and its own engine, no window and no audio device.
//!
//! Needs the `tracktion` feature. The CLAP test synth is built and copied into a scratch folder
//! that the clients' `CLAP_PATH` points at (an empty one for a client without it). Scratch files
//! go to `target/phase5-scratch/slice/`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use mdaw_server::{Config, Server};

const Q: i64 = 960;
const RATE: f64 = 48_000.0;
const SYNTH: &str = "clap:org.mdaw.test-synth";

fn target_dir() -> PathBuf {
    // .../target/debug/mdaw-slice
    Path::new(env!("CARGO_BIN_EXE_mdaw-slice"))
        .parent()
        .and_then(Path::parent)
        .expect("the binary is in target/<profile>/")
        .to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = target_dir().join("phase5-scratch/slice");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A folder with the test synth as a CLAP file. The test synth is a dev-dependency, so the build
/// of this test made its library (`deps/`). Only if it is missing is it built here: a second
/// cargo build of it during a test run would overwrite its rlib (a `cdylib` crate's files carry
/// no hash) under the feet of the run.
fn clap_dir() -> PathBuf {
    let mut built = target_dir().join("debug/deps/libmdaw_test_plugin.so");
    if !built.is_file() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "mdaw-test-plugin"])
            .status()
            .unwrap();
        assert!(status.success(), "building the test plugin");
        built = target_dir().join("debug/libmdaw_test_plugin.so");
    }
    let dir = scratch().join("clap");
    std::fs::create_dir_all(&dir).unwrap();
    // Copied to a new name and renamed, as tests running at once copy it too.
    let file = dir.join("mdaw-test-synth.clap");
    let part = dir.join(format!("mdaw-test-synth.clap.{:?}", std::thread::current().id()));
    if !file.is_file() || std::fs::read(&file).ok() != std::fs::read(&built).ok() {
        std::fs::copy(&built, &part).unwrap_or_else(|e| panic!("copy {}: {e}", built.display()));
        std::fs::rename(&part, &file).unwrap();
    }
    dir
}

fn empty_clap_dir() -> PathBuf {
    let dir = scratch().join("no-clap");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn serve(dir: &Path, listen: &str) -> Server {
    Server::start(Config {
        data_dir: dir.to_path_buf(),
        listen: listen.parse().unwrap(),
        host: None,
    })
    .unwrap()
}

// ---- a client process ------------------------------------------------------------------------

struct Client {
    name: String,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Client {
    fn start(server: &Server, name: &str, profile: &Path, clap: &Path) -> Client {
        Self::start_with(server, name, profile, clap, &[])
    }

    fn start_with(server: &Server, name: &str, profile: &Path, clap: &Path, extra: &[&str]) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mdaw-slice"))
            .args([
                "--join",
                &server.join_info().to_string(),
                "--name",
                name,
                "--profile",
            ])
            .arg(profile)
            .args(extra)
            .env("CLAP_PATH", clap)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mdaw-slice");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Client {
            name: name.into(),
            child,
            stdin,
            lines,
        };
        client.cmd("wait-scan");
        client
    }

    /// Runs a command; returns its `RESULT` lines as (key, value). Panics on an error.
    fn cmd(&mut self, line: &str) -> Vec<(String, String)> {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let mut results = Vec::new();
        loop {
            let got = self
                .lines
                .recv_timeout(Duration::from_secs(180))
                .unwrap_or_else(|_| panic!("{}: no answer to {line}", self.name));
            if got == format!("OK {line}") {
                return results;
            }
            if got.starts_with("ERR ") {
                panic!("{}: {got}", self.name);
            }
            if let Some(result) = got.strip_prefix("RESULT ") {
                for pair in result.split(' ') {
                    if let Some((k, v)) = pair.split_once('=') {
                        results.push((k.to_string(), v.to_string()));
                    }
                }
                // A notice is free text: keep it whole.
                if let Some(text) = result.strip_prefix("notice=") {
                    results.push(("notice".into(), text.into()));
                }
            }
        }
    }

    fn get(&mut self, line: &str, key: &str) -> String {
        self.cmd(line)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("{}: {line} gave no {key}", self.name))
    }

    fn seq(&mut self) -> u64 {
        self.get("settle", "seq").parse().unwrap()
    }

    fn wait_seq(&mut self, seq: u64) {
        let got: u64 = self.get(&format!("wait-seq {seq}"), "seq").parse().unwrap();
        assert_eq!(got, seq, "{}: seq", self.name);
    }

    fn hash(&mut self) -> String {
        self.get("doc-hash", "doc_hash")
    }

    fn render(&mut self, file: &str, beats: i64) -> Wav {
        let path = scratch().join(file);
        let _ = std::fs::remove_file(&path);
        self.cmd(&format!("render {} 0 {}", path.display(), beats * Q));
        read_wav(&path)
    }

    fn crash(mut self) {
        let _ = writeln!(self.stdin, "crash");
        let _ = self.stdin.flush();
        let status = self.child.wait().unwrap();
        assert!(!status.success(), "{}: crashed", self.name);
    }

    fn quit(mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("{}: did not quit", self.name);
    }
}

// ---- wav -----------------------------------------------------------------------------------------

struct Wav {
    samples: Vec<f32>,
}

fn read_wav(path: &Path) -> Wav {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let le32 = |p: usize| u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
    let mut pos = 12;
    let mut pcm = None;
    while pos + 8 <= data.len() {
        let size = le32(pos + 4).min(data.len() - pos - 8);
        if &data[pos..pos + 4] == b"data" {
            pcm = Some(&data[pos + 8..pos + 8 + size]);
        }
        pos += 8 + size + (size & 1);
    }
    let samples = pcm
        .expect("a data chunk")
        .chunks_exact(3)
        .map(|s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0)
        .collect();
    Wav { samples }
}

impl Wav {
    fn peak_between(&self, from: f64, to: f64) -> f64 {
        let frames = self.samples.len() / 2;
        let a = ((from * RATE) as usize).min(frames) * 2;
        let b = ((to * RATE) as usize).min(frames) * 2;
        self.samples[a..b.max(a)]
            .iter()
            .fold(0.0f64, |m, s| m.max(s.abs() as f64))
    }

    fn envelope(&self) -> Vec<f64> {
        let window = (0.05 * RATE) as usize * 2;
        self.samples
            .chunks(window)
            .map(|w| (w.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / w.len() as f64).sqrt())
            .collect()
    }
}

/// Two renders sound and are silent in the same 50 ms windows, at about the same level.
fn assert_same_sound(a: &Wav, b: &Wav, what: &str) -> f64 {
    let (ea, eb) = (a.envelope(), b.envelope());
    assert_eq!(ea.len(), eb.len(), "{what}: lengths");
    let mut sounding = 0;
    for (i, (x, y)) in ea.iter().zip(&eb).enumerate() {
        if *x > 0.01 || *y > 0.01 {
            assert!(
                *x > 0.001 && *y > 0.001,
                "{what}: window {i} sounds in one only ({x:.5} vs {y:.5})"
            );
            sounding += 1;
        }
    }
    assert!(sounding > 0, "{what}: nothing sounds");
    let energy = |e: &[f64]| e.iter().map(|x| x * x).sum::<f64>();
    let ratio = (energy(&ea) / energy(&eb)).sqrt();
    println!("RESULT {what}: sounding_windows={sounding} level_ratio={ratio:.4}");
    assert!((0.9..1.1).contains(&ratio), "{what}: level ratio {ratio:.3}");
    ratio
}

// ---- the song --------------------------------------------------------------------------------------

/// Keys: the built-in synth (one oscillator, so renders repeat), notes on beats 0 to 3 (0 to
/// 1.75 s at 120 bpm). Synth: the CLAP test synth, notes on beats 4 and 5 (2 to 2.75 s).
fn build_song(ann: &mut Client) -> u64 {
    for line in [
        "add-track Keys",
        "pocket-param Keys 4 0",
        "add-clip Keys k 0 4",
        "add-notes Keys k 0:60,1:64,2:67,3:72",
        "add-track Synth",
        "add-clip Synth s 4 4",
        "add-notes Synth s 0:48,1:55",
        &format!("add-plugin Synth {SYNTH}"),
    ] {
        ann.cmd(line);
    }
    ann.seq()
}

/// While the keys sound, and while only the synth does.
const KEYS: (f64, f64) = (0.0, 1.8);
const SYNTH_ONLY: (f64, f64) = (2.3, 3.0);

// ---- the tests ---------------------------------------------------------------------------------------

/// Points 1 to 4: the same song on both sides, concurrent edits, independent playback, and a
/// plugin setting that travels as state and does not come back.
#[test]
fn two_clients_build_play_and_tweak_one_song() {
    let started = Instant::now();
    let clap = clap_dir();
    let base = tempfile::tempdir_in(scratch()).unwrap();
    let server = serve(&base.path().join("server"), "127.0.0.1:0");
    let mut ann = Client::start(&server, "Ann", &base.path().join("ann"), &clap);
    let mut ben = Client::start(&server, "Ben", &base.path().join("ben"), &clap);

    // 1. Ann builds the song; Ben, who joined before, gets it and his own engine plays it.
    let seq = build_song(&mut ann);
    ben.wait_seq(seq);
    let hash = ann.hash();
    assert_eq!(ben.hash(), hash, "1: same document");
    println!("RESULT 1: seq={seq} doc_hash={hash}");
    for client in [&mut ann, &mut ben] {
        let status = client.cmd("device-status Synth");
        assert!(
            status.contains(&("track_sound".into(), "Live".into())),
            "{status:?}"
        );
    }
    let a = ann.render("1-ann.wav", 8);
    let b = ben.render("1-ben.wav", 8);
    for (who, wav) in [("ann", &a), ("ben", &b)] {
        println!(
            "RESULT 1: {who} keys_peak={:.4} synth_peak={:.4}",
            wav.peak_between(KEYS.0, KEYS.1),
            wav.peak_between(SYNTH_ONLY.0, SYNTH_ONLY.1)
        );
        assert!(wav.peak_between(KEYS.0, KEYS.1) > 0.05, "1: {who} hears the keys");
        assert!(
            wav.peak_between(SYNTH_ONLY.0, SYNTH_ONLY.1) > 0.02,
            "1: {who} hears the synth"
        );
    }
    assert_same_sound(&a, &b, "1: ann vs ben");
    let synth_before = b.peak_between(SYNTH_ONLY.0, SYNTH_ONLY.1);

    // 2. Concurrent edits: different notes from each, and the same note from both.
    ann.cmd("set-note Keys k 0 pitch 62");
    ben.cmd("set-note Keys k 1 pitch 65");
    ann.cmd("set-note Keys k 2 pitch 70");
    ben.cmd("set-note Keys k 2 pitch 71");
    let (sa, sb) = (ann.seq(), ben.seq());
    let head = sa.max(sb);
    ann.wait_seq(head);
    ben.wait_seq(head);
    assert_eq!(head, seq + 4, "2: four edits, each once");
    assert_eq!(ann.hash(), ben.hash(), "2: same document");
    let pitches: Vec<String> = (0..3)
        .map(|i| ann.get(&format!("note-pitch Keys k {i}"), "pitch"))
        .collect();
    let same_note = ben.get("note-pitch Keys k 2", "pitch");
    println!("RESULT 2: seq={head} pitches={pitches:?} same_note_ben={same_note}");
    assert_eq!(&pitches[..2], ["62", "65"], "2: both independent edits are in");
    assert_eq!(pitches[2], same_note, "2: one value for the note both changed");
    assert!(["70", "71"].contains(&same_note.as_str()));
    let a = ann.render("2-ann.wav", 8);
    let b = ben.render("2-ben.wav", 8);
    assert_same_sound(&a, &b, "2: ann vs ben");

    // 3. Playback is each one's own.
    ann.cmd("play");
    let state = |c: &mut Client| {
        let r = c.cmd("state");
        let get = |k: &str| r.iter().find(|(key, _)| key == k).unwrap().1.clone();
        (get("playing"), get("engine_playing"), get("position_tick"))
    };
    let (playing, engine_playing, _) = state(&mut ann);
    assert_eq!(
        (playing.as_str(), engine_playing.as_str()),
        ("true", "true"),
        "3: ann plays"
    );
    let (playing, _, _) = state(&mut ben);
    assert_eq!(playing, "false", "3: ben does not");
    ben.cmd(&format!("seek {}", 3 * Q));
    let (_, _, ben_at) = state(&mut ben);
    let (_, _, ann_at) = state(&mut ann);
    println!("RESULT 3: ben_position={ben_at} ann_position={ann_at}");
    assert_eq!(ben_at, (3 * Q).to_string(), "3: ben is where he went");
    assert_eq!(ann_at, "0", "3: ann's playhead did not move with his");
    ann.cmd("stop");

    // 4. Ann turns the synth's level down (as with the slider in the device tab); it reaches
    //    Ben as state, his engine plays it, and nothing comes back.
    ann.cmd("plugin-param Synth 0 0.2");
    let saved = ann.cmd("save-states");
    let seq4 = ann.seq();
    assert_eq!(seq4, head + 1, "4: one edit for the setting");
    ben.wait_seq(seq4);
    let state_a = ann.get("plugin-state Synth", "plugin_state");
    assert_ne!(state_a, "none");
    assert_eq!(
        ben.get("plugin-state Synth", "plugin_state"),
        state_a,
        "4: ben has it"
    );
    let b = ben.render("4-ben.wav", 8);
    let synth_after = b.peak_between(SYNTH_ONLY.0, SYNTH_ONLY.1);
    println!("RESULT 4: {saved:?} seq={seq4} synth_peak_before={synth_before:.4} after={synth_after:.4}");
    assert!(synth_after > 0.005, "4: still heard");
    assert!(synth_after < 0.6 * synth_before, "4: ben hears it quieter");
    let a = ann.render("4-ann.wav", 8);
    assert_same_sound(&a, &b, "4: ann vs ben");
    // No echo: after a while with both running, nobody has made another edit.
    ann.cmd("sleep 3000");
    ben.cmd("sleep 500");
    assert_eq!(ann.seq(), seq4, "4: no bounce on ann's side");
    assert_eq!(ben.seq(), seq4, "4: no bounce on ben's side");
    assert_eq!(ann.hash(), ben.hash(), "4: same document");

    ann.quit();
    ben.quit();
    server.stop();
    println!("RESULT points 1-4: {:?}", started.elapsed());
}

/// Point 5: Ann makes edits while the server is gone and crashes; she sends one more and
/// crashes at once. Restarted on the same profile she resends; each edit is in the log once.
#[test]
fn edits_survive_a_crash_and_arrive_once() {
    let started = Instant::now();
    let clap = clap_dir();
    let base = tempfile::tempdir_in(scratch()).unwrap();
    let data = base.path().join("server");
    let server = serve(&data, "127.0.0.1:0");
    let addr = server.local_addr().unwrap().to_string();
    let ann_profile = base.path().join("ann");
    let mut ann = Client::start(&server, "Ann", &ann_profile, &clap);
    let mut ben = Client::start(&server, "Ben", &base.path().join("ben"), &clap);
    let seq = build_song(&mut ann);
    ben.wait_seq(seq);

    // Offline: two edits, then a crash.
    server.stop();
    ann.cmd("sleep 300");
    ann.cmd("add-notes Keys k 0.5:61");
    ann.cmd("set-note Keys k 0 pitch 59");
    let pending: u64 = ann.get("seq", "unconfirmed").parse().unwrap();
    assert_eq!(pending, 2, "5: two edits waiting");
    ann.crash();

    let server = serve(&data, &addr);
    let mut ann = Client::start(&server, "Ann", &ann_profile, &clap);
    let after_restart = ann.seq();
    assert_eq!(after_restart, seq + 2, "5: the offline edits arrived once");
    ben.wait_seq(seq + 2);

    // Online: an edit, and a crash right after sending it.
    ann.cmd("add-notes Keys k 2.5:74");
    ann.crash();
    let mut ann = Client::start(&server, "Ann", &ann_profile, &clap);
    let last = ann.seq();
    ben.wait_seq(last);
    println!("RESULT 5: seq_before={seq} after_offline_crash={after_restart} after_online_crash={last}");
    assert_eq!(last, seq + 3, "5: every edit once");
    assert_eq!(ann.hash(), ben.hash(), "5: same document");
    let notes: Vec<String> = (0..6)
        .map(|i| ben.get(&format!("note-pitch Keys k {i}"), "pitch"))
        .collect();
    assert_eq!(
        notes,
        ["59", "61", "64", "67", "74", "72"],
        "5: the notes, each once"
    );
    let a = ann.render("5-ann.wav", 8);
    let b = ben.render("5-ben.wav", 8);
    assert_same_sound(&a, &b, "5: ann vs ben");
    ann.quit();
    ben.quit();
    server.stop();
    println!("RESULT point 5: {:?}", started.elapsed());
}

/// Point 6: Ben has not got the plugin. Same document; the track says so and is silent for him;
/// the rest plays.
#[test]
fn a_client_without_the_plugin_hears_the_rest() {
    let started = Instant::now();
    let clap = clap_dir();
    let base = tempfile::tempdir_in(scratch()).unwrap();
    let server = serve(&base.path().join("server"), "127.0.0.1:0");
    let mut ann = Client::start(&server, "Ann", &base.path().join("ann"), &clap);
    let mut ben = Client::start(&server, "Ben", &base.path().join("ben"), &empty_clap_dir());
    let seq = build_song(&mut ann);
    ben.wait_seq(seq);
    assert_eq!(ann.hash(), ben.hash(), "6: same document");
    let status = ben.cmd("device-status Synth");
    println!("RESULT 6: ben {status:?}");
    assert!(status.contains(&("device_status".into(), "NotInstalled".into())));
    assert!(status.contains(&("track_sound".into(), "NotInstalled".into())));
    let a = ann.render("6-ann.wav", 8);
    let b = ben.render("6-ben.wav", 8);
    let peaks = |w: &Wav| {
        (
            w.peak_between(KEYS.0, KEYS.1),
            w.peak_between(SYNTH_ONLY.0, SYNTH_ONLY.1),
        )
    };
    let (ann_keys, ann_synth) = peaks(&a);
    let (ben_keys, ben_synth) = peaks(&b);
    println!(
        "RESULT 6: ann keys={ann_keys:.4} synth={ann_synth:.4}; ben keys={ben_keys:.4} synth={ben_synth:.6}"
    );
    assert!(ann_synth > 0.02, "6: ann hears the synth");
    assert!(ben_synth < 1e-4, "6: ben hears no synth");
    assert!(
        ben_keys > 0.05 && (ben_keys / ann_keys - 1.0).abs() < 0.1,
        "6: ben hears the keys"
    );
    ann.quit();
    ben.quit();
    server.stop();
    println!("RESULT point 6: {:?}", started.elapsed());
}

/// Point 3 with audio devices: both clients play on their own device, and their playheads
/// move independently. Opens the default device twice and makes sound.
#[test]
#[ignore = "opens the audio device and makes sound"]
fn playheads_move_independently_on_devices() {
    let clap = clap_dir();
    let base = tempfile::tempdir_in(scratch()).unwrap();
    let server = serve(&base.path().join("server"), "127.0.0.1:0");
    let mut ann = Client::start_with(&server, "Ann", &base.path().join("ann"), &clap, &["--device"]);
    let mut ben = Client::start_with(&server, "Ben", &base.path().join("ben"), &clap, &["--device"]);
    let seq = build_song(&mut ann);
    ben.wait_seq(seq);
    let position = |c: &mut Client| -> (bool, i64) {
        let r = c.cmd("state");
        let get = |k: &str| r.iter().find(|(key, _)| key == k).unwrap().1.clone();
        (
            get("device_open") == "true",
            get("position_tick").parse().unwrap(),
        )
    };
    assert!(position(&mut ann).0 && position(&mut ben).0, "both devices open");
    ann.cmd("play");
    ann.cmd("sleep 1000");
    let (_, ann_1) = position(&mut ann);
    let (_, ben_1) = position(&mut ben);
    ben.cmd(&format!("seek {}", 8 * Q));
    ben.cmd("play");
    ben.cmd("sleep 1000");
    let (_, ann_2) = position(&mut ann);
    let (_, ben_2) = position(&mut ben);
    ann.cmd("stop");
    ben.cmd("stop");
    println!("RESULT 3 (devices): ann {ann_1} -> {ann_2}, ben {ben_1} -> {ben_2}");
    assert!(ann_1 > Q, "ann's playhead moves");
    assert_eq!(ben_1, 0, "ben's stands while ann plays");
    assert!(ann_2 > ann_1 + Q, "ann's goes on");
    assert!(
        ben_2 > 8 * Q + Q && ben_2 < 8 * Q + 4 * Q,
        "ben's moves from where he put it"
    );
    ann.quit();
    ben.quit();
    server.stop();
}
