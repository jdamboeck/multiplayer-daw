//! Playback is each client's own: two clients on one server play, stop, seek and loop
//! independently, and nothing about it goes over the network. What one records arrives at the
//! other as an ordinary edit.

use mdaw_client::{Session, Status, Who};
use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::*;
use mdaw_net::JoinInfo;
use mdaw_proto::{ClientMsg, ServerMsg};
use mdaw_server::{Config, Server};
use std::sync::Arc;
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

/// Everyone is online and has the same document.
fn settle(sessions: &mut [&mut Session]) {
    until(sessions, "the clients agree", |sessions| {
        let head = sessions.iter().map(|s| s.seq()).max().unwrap();
        sessions
            .iter()
            .all(|s| s.status() == Status::Online && s.unconfirmed() == 0 && s.seq() == head)
            && sessions.iter().all(|s| s.peers().len() == sessions.len())
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

/// Every message the protocol has. A message about playback would have to be added here, and
/// this test would not compile until it was.
fn kind_of(client: Option<&ClientMsg>, server: Option<&ServerMsg>) -> &'static str {
    let client = client.map(|msg| match msg {
        ClientMsg::Hello(_) => "hello",
        ClientMsg::Submit(_) => "submit",
        ClientMsg::Presence(_) => "presence",
    });
    let server = server.map(|msg| match msg {
        ServerMsg::Welcome(_) => "welcome",
        ServerMsg::Refused(_) => "refused",
        ServerMsg::Event(_) => "event",
        ServerMsg::Rejected { .. } => "rejected",
        ServerMsg::Peers(_) => "peers",
        ServerMsg::Presence { .. } => "presence",
    });
    client.or(server).unwrap_or("none")
}

#[test]
fn each_client_plays_on_its_own_and_tells_nobody() {
    assert_eq!(kind_of(None, None), "none");
    let (_dir, server) = serve();
    let (mut ann, mut ben) = (join(server.join_info(), "ann"), join(server.join_info(), "ben"));
    add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    let head = ann.seq();
    let doc = ann.project().clone();

    // Ann plays, loops and seeks; Ben's transport does not move.
    let range = TickRange::new(Tick::bars(2), Tick::bars(3));
    ann.set_loop(Some(range));
    ann.seek(Tick::bars(2));
    ann.play();
    until(&mut [&mut ann, &mut ben], "ann plays", |s| {
        s[0].position() > Tick::bars(2).0 as f64 + 100.0
    });
    run(&mut [&mut ann, &mut ben], Duration::from_millis(100));
    assert!(ann.is_playing());
    assert_eq!(ann.loop_range(), Some(range));
    assert!(!ben.is_playing(), "nobody else is started");
    assert_eq!(ben.position(), 0.0);
    assert_eq!(ben.loop_range(), None);

    // Ben plays from elsewhere and stops; Ann plays on, inside her loop.
    ben.seek(Tick::bars(8));
    ben.play();
    until(&mut [&mut ann, &mut ben], "ben plays", |s| {
        s[1].position() > Tick::bars(8).0 as f64 + 100.0
    });
    ben.pause();
    run(&mut [&mut ann, &mut ben], Duration::from_millis(50));
    assert!(ann.is_playing() && !ben.is_playing());
    let at = ann.position();
    assert!((range.start.0 as f64..range.end.0 as f64).contains(&at), "{at}");
    let rest = ben.position();
    run(&mut [&mut ann, &mut ben], Duration::from_millis(50));
    assert_eq!(ben.position(), rest, "stopped where Ben stopped it");

    // None of it reached the server: no event, and the document is as it was.
    assert_eq!((ann.seq(), ben.seq()), (head, head));
    assert_eq!(ann.project(), &doc);
    assert_eq!(ben.project(), &doc);
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
    // Where Ann's own playhead is when each note is played.
    let mut expected = Vec::new();
    for (pitch, hold) in [(60u8, 120u64), (64, 200), (67, 80)] {
        ann.frame();
        let start = ann.position();
        ann.live().press(pitch, 100);
        run(&mut [&mut ann, &mut ben], Duration::from_millis(hold));
        ann.frame();
        let end = ann.position();
        ann.live().release(pitch);
        run(&mut [&mut ann, &mut ben], Duration::from_millis(60));
        expected.push((pitch, start, end));
    }
    assert_eq!(ann.take_len(), 3);
    assert!(!ben.is_playing(), "Ann's playback is hers alone");
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
    // Within 10 ms of where they were played: 19.2 ticks at 120 bpm.
    let tolerance = 20.0;
    for (note, (pitch, start, end)) in notes.iter().zip(&expected) {
        assert_eq!(note.pitch, *pitch);
        let (got_start, got_end) = (note.start.0 as f64, (note.start + note.dur).0 as f64);
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
