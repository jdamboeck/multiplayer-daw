//! Live notes between real clients and a real server on loopback (M5): playing together, a
//! checked-out track that does not listen, a network that delays and loses, and recording.

use mdaw_client::{CommitState, Session, Status, Who};
use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::*;
use mdaw_net::JoinInfo;
use mdaw_server::{Config, Server};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn serve() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    };
    (dir, Server::start(config).unwrap())
}

fn join(join: &JoinInfo, name: &str) -> Session {
    let mut user_key = [0u8; 16];
    user_key[..name.len()].copy_from_slice(name.as_bytes());
    let who = Who {
        user_key,
        name: name.into(),
    };
    let engine = Engine::manual(48_000, true);
    Session::join(join.clone(), who, engine, Arc::new(|| {})).unwrap()
}

fn until(sessions: &mut [&mut Session], what: &str, done: impl Fn(&mut [&mut Session]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for session in sessions.iter_mut() {
            session.frame();
        }
        if done(sessions) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Runs frames for a while.
fn run(sessions: &mut [&mut Session], time: Duration) {
    let end = Instant::now() + time;
    while Instant::now() < end {
        for session in sessions.iter_mut() {
            session.frame();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Everyone is online, knows the server's clock, and has the same document and transport.
fn settle(sessions: &mut [&mut Session]) {
    until(sessions, "the clients agree", |sessions| {
        let head = sessions.iter().map(|s| s.seq()).max().unwrap();
        sessions.iter().all(|s| {
            s.status() == Status::Online && s.unconfirmed() == 0 && s.seq() == head && s.timing().is_some()
        }) && sessions.iter().all(|s| s.peers().len() == sessions.len())
            && sessions
                .windows(2)
                .all(|pair| pair[0].shared_transport() == pair[1].shared_transport())
    });
}

/// A Pocket track without clips.
fn add_track(session: &mut Session) -> TrackId {
    let ids = session.ids();
    let (track, device) = (ids.track(), ids.device());
    let mut content = TrackContent::default();
    let kind = DeviceKind::Builtin {
        kind: BuiltinKind::Pocket,
        dsp_version: pocket::DSP_VERSION,
    };
    content.devices.push_back(Device::new(device, kind));
    assert!(session.edit(vec![Op::TrackAdd {
        id: track,
        shell: TrackShell {
            name: "keys".into(),
            color: 0,
        },
        content,
        after: None,
    }]));
    track
}

/// Waits until the output of `session` is (or is not) above silence.
fn until_sound(sessions: &mut [&mut Session], which: usize, sounding: bool, what: &str) {
    until(sessions, what, |s| {
        let [left, right] = s[which].engine().take_peak();
        (left.max(right) > 0.01) == sounding
    });
}

#[test]
fn notes_one_plays_sound_at_once_there_and_soon_on_the_other_client() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(server.join_info(), "ann"), join(server.join_info(), "ben"));
    let track = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    for s in [&ann, &ben] {
        s.live().set_target(Some(track));
    }
    run(&mut [&mut ann, &mut ben], Duration::from_millis(50));
    for s in [&ann, &ben] {
        assert_eq!(s.engine().take_peak(), [0.0, 0.0], "silence before anyone plays");
    }

    ann.live().press(60, 100);
    ann.live().press(64, 100);
    assert_eq!(ann.live().held(), [(track, 60), (track, 64)]);
    until_sound(&mut [&mut ann, &mut ben], 0, true, "ann hears herself");
    until(&mut [&mut ann, &mut ben], "ben hears ann", |s| {
        s[1].live().others_sounding() == 2
    });
    assert_eq!(ben.players(track), ["ann"]);
    assert_eq!(ann.live().others_sounding(), 0, "nobody hears themselves twice");

    ann.live().release(60);
    until(&mut [&mut ann, &mut ben], "ben hears one note end", |s| {
        s[1].live().others_sounding() == 1
    });
    ann.live().release_all();
    until(&mut [&mut ann, &mut ben], "ben hears the other end", |s| {
        s[1].live().others_sounding() == 0
    });
    assert!(ben.players(track).is_empty());
    until_sound(&mut [&mut ann, &mut ben], 1, false, "ben's output dies away");

    // Both at once, on the same pitch: each ends only its own.
    ann.live().press(67, 100);
    ben.live().press(67, 100);
    until(&mut [&mut ann, &mut ben], "each hears the other", |s| {
        s.iter().all(|s| s.live().others_sounding() == 1)
    });
    ann.live().release(67);
    until(&mut [&mut ann, &mut ben], "ann's note ends on ben's side", |s| {
        s[1].live().others_sounding() == 0
    });
    assert_eq!(ben.live().held(), [(track, 67)]);
    assert_eq!(ann.live().others_sounding(), 1);

    // Someone who leaves stops playing.
    drop(ben);
    until(&mut [&mut ann], "ben's note ends when he leaves", |s| {
        s[0].live().others_sounding() == 0
    });
}

#[test]
fn a_checked_out_track_hears_nobody_else_until_the_commit() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(server.join_info(), "ann"), join(server.join_info(), "ben"));
    let track = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    for s in [&ann, &ben] {
        s.live().set_target(Some(track));
    }

    // Ben holds a note when Ann checks the track out: it ends on her side.
    ben.live().press(60, 100);
    until(&mut [&mut ann, &mut ben], "ann hears ben", |s| {
        s[0].live().others_sounding() == 1
    });
    ann.check_out(track);
    until(&mut [&mut ann, &mut ben], "the checkout lands", |s| {
        s[0].checkout_owner(track).is_some()
    });
    // At once, not only when Ben is not heard from any more.
    assert_eq!(ann.live().others_sounding(), 0, "ben's note ends for ann");
    until(&mut [&mut ann, &mut ben], "ben sees the checkout", |s| {
        s[1].checkout_owner(track).is_some()
    });

    // What Ben plays now does not reach her, while she still reaches him.
    ben.live().press(62, 100);
    ann.live().press(72, 100);
    until(&mut [&mut ann, &mut ben], "ben hears ann", |s| {
        s[1].live().others_sounding() == 1
    });
    run(&mut [&mut ann, &mut ben], Duration::from_millis(300));
    assert_eq!(ann.live().others_sounding(), 0, "the checkout does not listen");
    assert!(ann.players(track).is_empty());
    ann.live().release_all();
    ben.live().release_all();

    // After the commit the track listens again.
    assert!(ann.start_commit(track));
    until(&mut [&mut ann, &mut ben], "the commit is ready", |s| {
        matches!(s[0].commit_state(), Some(CommitState::Ready { .. }))
    });
    ann.finish_commit("", false);
    until(&mut [&mut ann, &mut ben], "the commit lands", |s| {
        s.iter().all(|s| s.checkout_owner(track).is_none())
    });
    ben.live().press(64, 100);
    until(&mut [&mut ann, &mut ben], "ann hears ben again", |s| {
        s[0].live().others_sounding() == 1
    });
    assert_eq!(ann.players(track), ["ben"]);
}

/// A bare connection that counts the live-note datagrams the server sends it.
struct Listener {
    heard: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Listener {
    fn join(join: &JoinInfo, name: &str) -> Listener {
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, welcomed) = std::sync::mpsc::channel();
        let (join, name) = (join.clone(), name.to_string());
        let (counter, stopped) = (heard.clone(), stop.clone());
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (_endpoint, connection) = mdaw_net::connect(&join).await.unwrap();
                let (mut send, mut recv) = connection.open_bi().await.unwrap();
                let mut user_key = [0u8; 16];
                user_key[..name.len()].copy_from_slice(name.as_bytes());
                let hello = mdaw_proto::ClientMsg::Hello(mdaw_proto::Hello {
                    proto_version: mdaw_proto::PROTO_VERSION,
                    token: join.token.clone(),
                    user_key,
                    name,
                    resume: None,
                });
                mdaw_net::write_frame(&mut send, &hello).await.unwrap();
                let welcome: Option<mdaw_proto::ServerMsg> = mdaw_net::read_frame(&mut recv).await.unwrap();
                assert!(matches!(welcome, Some(mdaw_proto::ServerMsg::Welcome(_))));
                ready.send(()).unwrap();
                while !stopped.load(Ordering::Relaxed) {
                    let read = mdaw_net::read_datagram(&connection);
                    if let Ok(Ok(Some(mdaw_proto::Datagram::Midi(_)))) =
                        tokio::time::timeout(Duration::from_millis(20), read).await
                    {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        });
        welcomed.recv_timeout(Duration::from_secs(10)).unwrap();
        Listener { heard, stop }
    }

    fn heard(&self) -> usize {
        self.heard.load(Ordering::Relaxed)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn the_server_does_not_pass_others_notes_to_the_owner_of_a_checkout() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(server.join_info(), "ann"), join(server.join_info(), "ben"));
    // A second connection of Ann's: it gets what the server sends her, without her client
    // filtering anything.
    let ears = Listener::join(server.join_info(), "ann");
    let track = add_track(&mut ann);
    until(&mut [&mut ann, &mut ben], "everyone is there", |s| {
        s.iter().all(|s| s.peers().len() == 3 && s.unconfirmed() == 0)
    });
    ben.live().set_target(Some(track));
    ben.live().press(60, 100);
    until(&mut [&mut ann, &mut ben], "ann's ears hear ben", |_| {
        ears.heard() > 0
    });
    ben.live().release(60);

    ann.check_out(track);
    until(&mut [&mut ann, &mut ben], "ben sees the checkout", |s| {
        s[1].checkout_owner(track).is_some()
    });
    run(&mut [&mut ann, &mut ben], Duration::from_millis(1200));
    let before = ears.heard();
    for pitch in 60..70 {
        ben.live().press(pitch, 100);
        run(&mut [&mut ann, &mut ben], Duration::from_millis(30));
    }
    ben.live().release_all();
    run(&mut [&mut ann, &mut ben], Duration::from_millis(500));
    assert_eq!(ears.heard(), before, "nothing of ben's reached ann");
    // Others still hear him: Ben's notes on a track Ann has not checked out reach her.
    let other = add_track(&mut ben);
    until(&mut [&mut ann, &mut ben], "the track is everywhere", |s| {
        s[0].project().track(other).is_some()
    });
    ben.live().set_target(Some(other));
    ben.live().press(60, 100);
    until(&mut [&mut ann, &mut ben], "ann's ears hear ben elsewhere", |_| {
        ears.heard() > before
    });
}

/// A UDP relay that stands in for `tc netem`: everything between one client and the server
/// arrives `delay` late, and `loss_percent` of it never.
struct BadNetwork {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl BadNetwork {
    fn start(server: SocketAddr, delay: Duration, loss_percent: u64) -> BadNetwork {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (queue, queued) = std::sync::mpsc::channel::<(Instant, SocketAddr, Vec<u8>)>();
        let sender = socket.try_clone().unwrap();
        // The delay is the same for everything, so packets leave in the order they came.
        std::thread::spawn(move || {
            for (due, to, bytes) in queued {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                let _ = sender.send_to(&bytes, to);
            }
        });
        let stopped = stop.clone();
        std::thread::spawn(move || {
            let mut client: Option<SocketAddr> = None;
            let mut rng = 0x2545_f491_4f6c_dd1du64;
            let mut buf = [0u8; 65536];
            while !stopped.load(Ordering::Relaxed) {
                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                let to = if from == server {
                    match client {
                        Some(client) => client,
                        None => continue,
                    }
                } else {
                    client = Some(from);
                    server
                };
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if rng % 100 < loss_percent {
                    continue;
                }
                let _ = queue.send((Instant::now() + delay, to, buf[..len].to_vec()));
            }
        });
        BadNetwork { addr, stop }
    }
}

impl Drop for BadNetwork {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn over_a_slow_and_lossy_network_no_note_is_left_hanging() {
    let (_dir, server) = serve();
    let server_addr = server.local_addr().unwrap();
    // Ben is behind the bad network; Ann plays.
    let bad = BadNetwork::start(server_addr, Duration::from_millis(80), 5);
    let mut far = server.join_info().clone();
    far.host = "127.0.0.1".into();
    far.port = bad.addr.port();
    let mut ann = join(server.join_info(), "ann");
    let mut ben = join(&far, "ben");
    let track = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    ann.live().set_target(Some(track));

    // A quick and dense run of notes, many of them overlapping.
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut most = 0;
    for _ in 0..300 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let pitch = 48 + (rng % 24) as u8;
        if ann.live().held().iter().any(|(_, p)| *p == pitch) {
            ann.live().release(pitch);
        } else {
            ann.live().press(pitch, 90);
        }
        run(&mut [&mut ann, &mut ben], Duration::from_millis(rng % 15));
        most = most.max(ben.live().others_sounding());
        assert!(ben.live().others_sounding() <= 24);
    }
    assert!(most > 0, "ben heard ann at all");
    ann.live().release_all();
    let released = Instant::now();
    until(&mut [&mut ann, &mut ben], "every note ends for ben", |s| {
        s[1].live().others_sounding() == 0
    });
    // With the releases repeated, they get through long before the two-second timeout.
    let took = released.elapsed();
    eprintln!(
        "the last note ended for ben {} ms after ann let go",
        took.as_millis()
    );
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    until_sound(&mut [&mut ann, &mut ben], 1, false, "ben's output dies away");
}

#[test]
fn a_recording_lands_on_the_other_client_where_it_was_played() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(server.join_info(), "ann"), join(server.join_info(), "ben"));
    let track = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    ann.live().set_target(Some(track));

    ann.set_recording(true);
    ann.play();
    until(&mut [&mut ann, &mut ben], "the transport runs", |s| {
        s[0].position() > 100.0
    });
    // Where the shared playhead is by the server's own clock when each note is played.
    let at = |ann: &Session| {
        let state = ann.shared_transport().unwrap();
        state.position(server.clock().micros(), &ann.project().tempo_map())
    };
    let mut expected = Vec::new();
    for (pitch, hold) in [(60u8, 120u64), (64, 200), (67, 80)] {
        let start = at(&ann);
        ann.live().press(pitch, 100);
        run(&mut [&mut ann, &mut ben], Duration::from_millis(hold));
        let end = at(&ann);
        ann.live().release(pitch);
        run(&mut [&mut ann, &mut ben], Duration::from_millis(60));
        expected.push((pitch, start, end));
    }
    assert_eq!(ann.take_len(), 3);
    ann.pause();
    settle(&mut [&mut ann, &mut ben]);

    let clips = &ben.project().track(track).unwrap().content.clips;
    assert_eq!(clips.len(), 1, "one take, one clip");
    let clip = clips.values().next().unwrap();
    assert_eq!(clip.name, "Take");
    assert_eq!(clip.start, Tick::ZERO);
    let mut notes: Vec<&Note> = clip.notes.values().collect();
    notes.sort_by_key(|n| n.start);
    assert_eq!(notes.len(), 3);
    // Within 5 ms of where they were played: 9.6 ticks at 120 bpm.
    let tolerance = 10.0;
    for (note, (pitch, start, end)) in notes.iter().zip(&expected) {
        assert_eq!(note.pitch, *pitch);
        let (got_start, got_end) = (note.start.0 as f64, (note.start + note.dur).0 as f64);
        eprintln!("pitch {pitch}: {got_start}..{got_end}, played {start:.1}..{end:.1}");
        assert!(
            (got_start - start).abs() < tolerance,
            "pitch {pitch} starts at {got_start}, played at {start:.1}"
        );
        assert!(
            (got_end - end).abs() < tolerance,
            "pitch {pitch} ends at {got_end}, let go at {end:.1}"
        );
    }
    assert_eq!(ann.project().track(track), ben.project().track(track));

    // The take is one undo step.
    assert!(ann.undo());
    settle(&mut [&mut ann, &mut ben]);
    assert!(ben.project().track(track).unwrap().content.clips.is_empty());

    // Nothing is recorded while stopped, or once recording is off.
    ann.live().press(60, 100);
    ann.live().release(60);
    run(&mut [&mut ann, &mut ben], Duration::from_millis(50));
    ann.set_recording(false);
    ann.play();
    until(&mut [&mut ann, &mut ben], "the transport runs", |s| {
        s[0].position() > 100.0
    });
    ann.live().press(62, 100);
    run(&mut [&mut ann, &mut ben], Duration::from_millis(50));
    ann.live().release(62);
    ann.pause();
    settle(&mut [&mut ann, &mut ben]);
    assert!(ben.project().track(track).unwrap().content.clips.is_empty());
}
