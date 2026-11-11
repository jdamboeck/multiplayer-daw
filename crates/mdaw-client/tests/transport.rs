//! The shared transport between real clients and a real server on loopback. The engines have
//! no audio device, but keep time against the wall clock as one would.

use mdaw_client::{Session, Status, Who};
use mdaw_engine::Engine;
use mdaw_model::{MixProp, Op, TempoPoint, Tick, TickRange, TrackContent, TrackShell};
use mdaw_server::{Config, Server};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What "together" means here: the plan's 10 ms.
const TOGETHER_MICROS: f64 = 10_000.0;

fn serve() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    };
    let server = Server::start(config).unwrap();
    (dir, server)
}

fn join(server: &Server, name: &str) -> Session {
    join_with(server, name, Engine::manual(48_000, true))
}

fn join_with(server: &Server, name: &str, engine: Engine) -> Session {
    let mut user_key = [0u8; 16];
    user_key[..name.len()].copy_from_slice(name.as_bytes());
    let who = Who {
        user_key,
        name: name.into(),
    };
    Session::join(server.join_info().clone(), who, engine, Arc::new(|| {})).unwrap()
}

/// Runs frames on all sessions until `done`, as their windows would.
fn until(sessions: &mut [&mut Session], what: &str, done: impl Fn(&[&mut Session]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
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

/// Everyone is connected, knows the server's clock, and has heard the same about the
/// transport and the document, with nothing of their own still on the way.
fn settle(sessions: &mut [&mut Session]) {
    // Answers to what was just asked need a moment to exist at all.
    let asked = Instant::now();
    until(sessions, "the clients agree", |sessions| {
        asked.elapsed() > Duration::from_millis(50)
            && sessions
                .iter()
                .all(|s| s.status() == Status::Online && s.unconfirmed() == 0 && s.linked().is_some())
            && sessions
                .iter()
                .all(|s| s.timing().is_some() || s.linked() == Some(false))
            && sessions.windows(2).all(|pair| {
                pair[0].shared_transport() == pair[1].shared_transport()
                    && pair[0].project() == pair[1].project()
            })
    });
}

/// How far each session's playhead is from where the shared transport is by the server's own
/// clock, in microseconds: the median of many looks over `span`.
fn apart(server: &Server, sessions: &mut [&mut Session], span: Duration) -> Vec<f64> {
    let mut off: Vec<Vec<f64>> = vec![Vec::new(); sessions.len()];
    let end = Instant::now() + span;
    while Instant::now() < end {
        for (session, off) in sessions.iter_mut().zip(&mut off) {
            session.frame();
            let now = server.clock().micros();
            let position = session.position();
            let tempo = session.project().tempo_map();
            let state = session.shared_transport().unwrap();
            let expected = state.position(now, &tempo);
            let ticks_per_micro = (tempo.seconds_to_tick(1.0) - tempo.seconds_to_tick(0.0)) / 1e6;
            off.push((position - expected) / ticks_per_micro);
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    off.into_iter()
        .map(|mut off| {
            off.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
            off[off.len() / 2]
        })
        .collect()
}

fn assert_together(server: &Server, sessions: &mut [&mut Session]) {
    for (i, off) in apart(server, sessions, Duration::from_millis(300))
        .into_iter()
        .enumerate()
    {
        eprintln!("client {i}: {off:+.0} µs from the shared playhead");
        assert!(
            off.abs() < TOGETHER_MICROS,
            "client {i} is {off:.0} µs from the shared playhead"
        );
        // The engine's own measure, against its estimate of the server's clock.
        let timing = sessions[i].timing().unwrap();
        let drift = timing.drift_micros.expect("playing") as f64;
        assert!(drift.abs() < TOGETHER_MICROS, "client {i} drifts by {drift} µs");
    }
}

#[test]
fn play_starts_everyone_together_and_pause_stops_them_in_one_place() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    settle(&mut [&mut ann, &mut ben]);
    assert!(!ann.is_playing() && !ben.is_playing());

    ann.play();
    assert!(ann.is_playing(), "the one who pressed play sees it at once");
    settle(&mut [&mut ann, &mut ben]);
    assert!(ben.is_playing());
    until(&mut [&mut ann, &mut ben], "both are under way", |s| {
        s.iter().all(|s| s.position() > 0.0)
    });
    assert_together(&server, &mut [&mut ann, &mut ben]);
    for session in [&ann, &ben] {
        assert_eq!(session.timing().unwrap().resyncs, 0, "nobody had to jump");
    }

    // Either may stop it.
    ben.pause();
    settle(&mut [&mut ann, &mut ben]);
    assert!(!ann.is_playing() && !ben.is_playing());
    until(&mut [&mut ann, &mut ben], "both have come to rest", |s| {
        s[0].position() == s[1].position()
    });
    let at = ann.shared_transport().unwrap().anchor_tick;
    assert!(at > 0.0 && at.fract() == 0.0, "stopped on a whole tick, at {at}");
    assert_eq!(ann.position(), at);
}

#[test]
fn seeking_and_the_loop_are_shared() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    settle(&mut [&mut ann, &mut ben]);

    ben.seek(Tick::bars(2));
    // A request is the whole state, and the last one wins as a whole: let this one land first.
    settle(&mut [&mut ann, &mut ben]);
    let range = TickRange::new(Tick::bars(1), Tick::bars(3));
    ann.set_loop(Some(range));
    settle(&mut [&mut ann, &mut ben]);
    until(&mut [&mut ann, &mut ben], "both are at bar 3", |s| {
        s.iter().all(|s| s.position() == Tick::bars(2).0 as f64)
    });
    assert_eq!(ann.loop_range(), Some(range));
    assert_eq!(ben.loop_range(), Some(range));

    // Seeking while it plays moves everyone, and the loop holds them.
    ann.play();
    settle(&mut [&mut ann, &mut ben]);
    ben.seek(Tick::bars(1));
    settle(&mut [&mut ann, &mut ben]);
    until(&mut [&mut ann, &mut ben], "both play from bar 2", |s| {
        s.iter()
            .all(|s| (Tick::bars(1).0 as f64..Tick::bars(2).0 as f64).contains(&s.position()))
    });
    assert_together(&server, &mut [&mut ann, &mut ben]);

    ann.set_loop(None);
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ben.loop_range(), None);
    assert_together(&server, &mut [&mut ann, &mut ben]);
}

#[test]
fn a_tempo_change_while_playing_leaves_everyone_together() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    settle(&mut [&mut ann, &mut ben]);
    ann.play();
    settle(&mut [&mut ann, &mut ben]);
    until(&mut [&mut ann, &mut ben], "half a beat has played", |s| {
        s.iter().all(|s| s.position() > 480.0)
    });

    let before = ben.position();
    let id = *ben.project().tempo.keys().next().unwrap();
    let point = TempoPoint {
        tick: Tick::ZERO,
        milli_bpm: 60_000,
    };
    assert!(ben.edit(vec![Op::TempoPut { id, point }]));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ann.project().tempo_map().milli_bpm_at(Tick::ZERO), 60_000);
    assert_together(&server, &mut [&mut ann, &mut ben]);
    // The playhead went on from where it was, at the new pace: at most a second has passed,
    // which is one beat now.
    for session in [&ann, &ben] {
        let moved = session.position() - before;
        assert!(
            (0.0..1500.0).contains(&moved),
            "the playhead moved by {moved} ticks"
        );
    }
}

#[test]
fn a_refused_tempo_change_leaves_the_playhead_where_the_server_has_it() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let track = ann.ids().track();
    let add = Op::TrackAdd {
        id: track,
        shell: TrackShell {
            name: "t".into(),
            color: 0,
        },
        content: TrackContent::default(),
        after: None,
    };
    assert!(ann.edit(vec![add]));
    settle(&mut [&mut ann, &mut ben]);
    ann.play();
    settle(&mut [&mut ann, &mut ben]);
    until(&mut [&mut ann, &mut ben], "half a beat has played", |s| {
        s.iter().all(|s| s.position() > 480.0)
    });

    // Ben checks the track out; before Ann hears of it, she changes the tempo and the track in
    // one edit, which the server refuses as a whole.
    ben.check_out(track);
    until(&mut [&mut ben], "the checkout is Ben's", |s| {
        s[0].workspace(track).is_some()
    });
    let id = *ann.project().tempo.keys().next().unwrap();
    let point = TempoPoint {
        tick: Tick::ZERO,
        milli_bpm: 60_000,
    };
    let gain = Op::TrackSetMix {
        track,
        prop: MixProp::Gain(0.5),
    };
    assert!(ann.edit(vec![Op::TempoPut { id, point }, gain]));
    until(&mut [&mut ann, &mut ben], "the edit is refused", |s| {
        s[0].notices().iter().any(|n| n.contains("not accepted"))
    });
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ann.project().tempo_map().milli_bpm_at(Tick::ZERO), 120_000);
    assert_together(&server, &mut [&mut ann, &mut ben]);
}

#[test]
fn an_unlinked_client_plays_on_its_own() {
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ben.linked(), Some(true));

    ben.set_linked(false);
    ann.play();
    settle(&mut [&mut ann, &mut ben]);
    assert!(ann.is_playing());
    assert!(!ben.is_playing(), "an unlinked client is not started by others");
    assert_eq!(ben.position(), 0.0);

    // Its own transport moves nobody else's.
    ben.seek(Tick::bars(8));
    ben.play();
    ann.pause();
    settle(&mut [&mut ann, &mut ben]);
    until(&mut [&mut ann, &mut ben], "ben plays on", |s| {
        s[1].position() > Tick::bars(8).0 as f64
    });
    assert!(ben.is_playing() && !ann.is_playing());
    assert!(ann.position() < Tick::bars(8).0 as f64);

    // Linking again puts it where everyone is.
    ben.set_linked(true);
    settle(&mut [&mut ann, &mut ben]);
    assert!(!ben.is_playing());
    until(&mut [&mut ann, &mut ben], "ben is back with ann", |s| {
        s[0].position() == s[1].position()
    });

    // Unlinking while the shared transport plays goes on from there, alone.
    ann.play();
    settle(&mut [&mut ann, &mut ben]);
    ben.set_linked(false);
    ann.pause();
    settle(&mut [&mut ann, &mut ben]);
    let was = ben.position();
    until(&mut [&mut ann, &mut ben], "ben plays on", |s| {
        s[1].position() > was + 100.0
    });
    assert!(ben.is_playing() && !ann.is_playing());
}

#[test]
fn someone_who_joins_while_it_plays_is_where_the_others_are() {
    let (_dir, server) = serve();
    let mut ann = join(&server, "ann");
    settle(&mut [&mut ann]);
    ann.set_loop(Some(TickRange::new(Tick::ZERO, Tick::beats(2))));
    ann.play();
    settle(&mut [&mut ann]);
    until(&mut [&mut ann], "the loop has come round", |s| {
        s[0].shared_transport().is_some_and(|t| t.playing) && s[0].position() > 0.0
    });
    std::thread::sleep(Duration::from_millis(1100));

    let mut ben = join(&server, "ben");
    settle(&mut [&mut ann, &mut ben]);
    assert!(ben.is_playing());
    assert_eq!(ben.loop_range(), ann.loop_range());
    until(&mut [&mut ann, &mut ben], "ben is under way", |s| {
        s[1].position() > 0.0
    });
    assert_together(&server, &mut [&mut ann, &mut ben]);
}

#[test]
fn the_transport_stops_when_everyone_has_left() {
    let (_dir, server) = serve();
    let mut ann = join(&server, "ann");
    settle(&mut [&mut ann]);
    ann.play();
    settle(&mut [&mut ann]);
    until(&mut [&mut ann], "it plays", |s| s[0].position() > 100.0);
    drop(ann);

    // Nobody listened, so nothing went on: the next one finds it stopped where it was left.
    std::thread::sleep(Duration::from_millis(300));
    let mut ben = join(&server, "ben");
    settle(&mut [&mut ben]);
    assert!(!ben.is_playing());
    let at = ben.shared_transport().unwrap().anchor_tick;
    assert!(at > 100.0 && at < 1500.0, "stopped at {at}");
    until(&mut [&mut ben], "ben is there", |s| s[0].position() == at);
}

/// The same with two real audio streams, whose callbacks come in bursts and say themselves
/// when their buffers will be heard. Plays silence for a few seconds. Run with
/// `cargo test -p mdaw-client --test transport -- --ignored --nocapture devices`.
#[test]
#[ignore = "needs an audio device"]
fn two_audio_devices_stay_together() {
    let (_dir, server) = serve();
    let open = || Engine::open().expect("an audio device");
    let (mut ann, mut ben) = (
        join_with(&server, "ann", open()),
        join_with(&server, "ben", open()),
    );
    eprintln!("{} / {}", ann.engine().description(), ben.engine().description());
    settle(&mut [&mut ann, &mut ben]);
    ann.play();
    settle(&mut [&mut ann, &mut ben]);

    let mut worst = [0i64; 2];
    let end = Instant::now() + Duration::from_secs(4);
    let mut said = Instant::now();
    while Instant::now() < end {
        for (session, worst) in [&mut ann, &mut ben].into_iter().zip(&mut worst) {
            session.frame();
            if let Some(drift) = session.timing().and_then(|timing| timing.drift_micros) {
                *worst = (*worst).max(drift.abs());
            }
        }
        if said.elapsed() > Duration::from_millis(500) {
            said = Instant::now();
            let drift = |s: &Session| s.timing().unwrap().drift_micros;
            eprintln!(
                "ann at {:.0} ({:?} µs), ben at {:.0} ({:?} µs)",
                ann.position(),
                drift(&ann),
                ben.position(),
                drift(&ben)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for (session, worst) in [&ann, &ben].into_iter().zip(worst) {
        let timing = session.timing().unwrap();
        eprintln!(
            "worst {worst} µs, {} resyncs, {:?}",
            timing.resyncs,
            session.engine().stats()
        );
        assert_eq!(timing.resyncs, 0, "nobody had to jump");
        assert!((worst as f64) < TOGETHER_MICROS, "off by up to {worst} µs");
    }
}

#[test]
fn without_the_server_each_plays_alone_and_a_new_server_starts_stopped() {
    let (dir, server) = serve();
    let addr = server.local_addr().unwrap();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let track = ann.ids().track();
    let add = Op::TrackAdd {
        id: track,
        shell: TrackShell {
            name: "t".into(),
            color: 0,
        },
        content: TrackContent::default(),
        after: None,
    };
    assert!(ann.edit(vec![add]));
    ann.set_loop(Some(TickRange::new(Tick::ZERO, Tick::bars(1))));
    settle(&mut [&mut ann, &mut ben]);
    ann.play();
    settle(&mut [&mut ann, &mut ben]);

    // The server goes away: both play on, and each transport is its own.
    server.stop();
    until(&mut [&mut ann, &mut ben], "both notice", |s| {
        s.iter().all(|s| s.status() == Status::Reconnecting)
    });
    assert!(ann.is_playing() && ben.is_playing());
    assert!(ann.timing().is_none(), "no shared clock without the server");
    ann.pause();
    until(&mut [&mut ann, &mut ben], "a moment passes", |s| {
        s[1].position() > 960.0
    });
    assert!(!ann.is_playing() && ben.is_playing(), "only ann stops");

    // A new server knows nothing of the old transport: everyone stopped at the start, no loop.
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: addr,
        host: None,
    };
    let _server = Server::start(config).unwrap();
    settle(&mut [&mut ann, &mut ben]);
    for session in [&ann, &ben] {
        assert!(!session.is_playing());
        assert_eq!(session.position(), 0.0);
        assert_eq!(session.loop_range(), None);
        assert!(session.project().track(track).is_some(), "the document is kept");
    }
    ben.play();
    settle(&mut [&mut ann, &mut ben]);
    assert!(ann.is_playing() && ben.is_playing());
}
