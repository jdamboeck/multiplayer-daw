//! A real server and real clients in one process, talking QUIC over loopback. Only the audio
//! device and the window are missing.

use mdaw_client::{Session, Status, Who};
use mdaw_engine::Engine;
use mdaw_model::{
    Clip, ClipId, ClipProp, EntityRef, MixProp, Note, NoteId, NoteProp, Op, Project, Tick, TrackContent,
    TrackId, TrackShell,
};
use mdaw_server::{Config, Server};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn serve(dir: &Path, listen: &str) -> Server {
    let config = Config {
        data_dir: dir.to_path_buf(),
        listen: listen.parse().unwrap(),
        host: None,
    };
    Server::start(config).unwrap()
}

fn who(name: &str) -> Who {
    let mut user_key = [0u8; 16];
    user_key[..name.len()].copy_from_slice(name.as_bytes());
    Who {
        user_key,
        name: name.into(),
    }
}

fn join(server: &Server, name: &str) -> Session {
    let engine = Engine::manual(48_000, false);
    Session::join(server.join_info().clone(), who(name), engine, Arc::new(|| {})).unwrap()
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

/// Everything sent has been confirmed and everyone holds the same document.
fn settle(sessions: &mut [&mut Session]) {
    until(sessions, "the clients agree", |sessions| {
        let head = sessions.iter().map(|s| s.seq()).max().unwrap();
        sessions
            .iter()
            .all(|s| s.unconfirmed() == 0 && s.seq() == head && s.status() == Status::Online)
            && sessions
                .windows(2)
                .all(|pair| pair[0].project() == pair[1].project())
    });
}

fn note(pitch: u8) -> Note {
    Note {
        start: Tick::ZERO,
        dur: Tick::beats(1),
        pitch,
        vel: 100,
    }
}

/// A track with one clip holding one note.
fn add_track(session: &mut Session, name: &str) -> (TrackId, ClipId, NoteId) {
    let ids = session.ids();
    let (track, clip, id) = (ids.track(), ids.clip(), ids.note());
    let shell = TrackShell {
        name: name.into(),
        color: 0x4FA3E0,
    };
    let added = session.edit(vec![
        Op::TrackAdd {
            id: track,
            shell,
            content: TrackContent::default(),
            after: None,
        },
        Op::ClipAdd {
            track,
            id: clip,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(1),
                name: "clip".into(),
                notes: Default::default(),
            },
        },
        Op::NoteAdd {
            track,
            clip,
            id,
            note: note(60),
        },
    ]);
    assert!(added);
    (track, clip, id)
}

fn set_pitch(session: &mut Session, at: (TrackId, ClipId, NoteId), pitch: u8) -> bool {
    let (track, clip, id) = at;
    session.edit(vec![Op::NoteSet {
        track,
        clip,
        id,
        prop: NoteProp::Pitch(pitch),
    }])
}

fn pitch(project: &Project, at: (TrackId, ClipId, NoteId)) -> Option<u8> {
    let (track, clip, id) = at;
    Some(
        project
            .track(track)?
            .content
            .clips
            .get(&clip)?
            .notes
            .get(&id)?
            .pitch,
    )
}

#[test]
fn edits_reach_the_other_client() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    assert_ne!(ann.user(), ben.user());
    assert_ne!(ann.actor(), ben.actor());

    let at = add_track(&mut ann, "Lead");
    assert_eq!(pitch(ann.project(), at), Some(60), "an edit shows at once");
    assert_eq!(ann.unconfirmed(), 1);
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(60));

    assert!(set_pitch(&mut ben, at, 64));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ann.project(), at), Some(64));

    // Undo travels like any other edit.
    assert!(ben.undo());
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ann.project(), at), Some(60));

    let names = |s: &Session| s.peers().iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&ann), ["ann", "ben"]);
    assert_eq!(names(&ben), ["ann", "ben"]);
    assert_eq!(ann.user_name(ben.user()), "ben");
    assert_eq!(ann.user_name(ann.user()), "you");
    assert!(ann.notices().is_empty() && ben.notices().is_empty());
}

#[test]
fn a_client_that_was_gone_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let at = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);
    let ben_was = ben.user();

    drop(ben);
    until(&mut [&mut ann], "ben is seen leaving", |s| {
        s[0].peers().len() == 1
    });
    set_pitch(&mut ann, at, 72);
    let second = add_track(&mut ann, "Bass");

    let mut ben = join(&server, "ben");
    assert_eq!(ben.user(), ben_was, "the same install is the same user");
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(72));
    assert_eq!(pitch(ben.project(), second), Some(60));
    // Who is there travels apart from the document, so it may come a moment later.
    until(&mut [&mut ann, &mut ben], "ann sees ben back", |s| {
        s[0].peers().len() == 2
    });

    // The new connection mints IDs that do not collide with the old one's.
    let third = add_track(&mut ben, "Pad");
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ann.project().track_order.len(), 3);
    assert_eq!(pitch(ann.project(), third), Some(60));
}

#[test]
fn the_server_restarts_under_its_clients() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let addr: SocketAddr = server.local_addr().unwrap();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let at = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);
    let before = ann.seq();

    server.stop();
    until(
        &mut [&mut ann, &mut ben],
        "both notice the server is gone",
        |sessions| sessions.iter().all(|s| s.status() == Status::Reconnecting),
    );

    // Work goes on without the server, and is kept.
    assert!(set_pitch(&mut ann, at, 67));
    let offline = add_track(&mut ben, "Made offline");
    assert_eq!(pitch(ann.project(), at), Some(67));
    assert_eq!((ann.unconfirmed(), ben.unconfirmed()), (1, 1));

    let server = serve(dir.path(), &addr.to_string());
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(67));
    assert_eq!(pitch(ann.project(), offline), Some(60));
    assert_eq!(ann.seq(), before + 2, "each offline edit was sequenced once");
    until(&mut [&mut ann, &mut ben], "ann sees ben back", |s| {
        s[0].peers().len() == 2
    });

    // What was made before the restart can still be undone after it.
    assert!(ann.undo());
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(60));
    drop(server);
}

#[test]
fn a_refused_edit_is_taken_back_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let at = add_track(&mut ann, "Lead");
    let bass = add_track(&mut ann, "Bass");
    settle(&mut [&mut ann, &mut ben]);

    // Ben takes the bass out of live editing. Ann has not heard of that yet when she edits it,
    // together with the lead: the server turns down the whole edit.
    ben.check_out(bass.0);
    until(&mut [&mut ben], "ben's checkout is in the log", |s| {
        s[0].workspace(bass.0).is_some()
    });
    let (track, clip, id) = at;
    assert!(ann.edit(vec![
        Op::NoteSet {
            track,
            clip,
            id,
            prop: NoteProp::Pitch(90)
        },
        Op::NoteSet {
            track: bass.0,
            clip: bass.1,
            id: bass.2,
            prop: NoteProp::Pitch(90)
        },
    ]));
    assert_eq!(pitch(ann.project(), at), Some(90));
    // An edit made on top of the refused one survives it.
    assert!(ann.edit(vec![Op::NoteSet {
        track,
        clip,
        id,
        prop: NoteProp::Vel(33)
    }]));

    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(
        pitch(ann.project(), at),
        Some(60),
        "nothing of the refused edit stays"
    );
    assert_eq!(pitch(ann.project(), bass), Some(60));
    assert_eq!(
        ben.project().track(track).unwrap().content.clips[&clip].notes[&id].vel,
        33
    );
    assert_eq!(ann.notices().len(), 1, "{:?}", ann.notices());
    assert!(ann.notices()[0].contains("not accepted"), "{:?}", ann.notices());
    assert!(ben.notices().is_empty());

    ann.dismiss_notice(0);
    assert!(ann.notices().is_empty());
}

#[test]
fn an_edit_displaced_by_a_concurrent_one_can_be_restored() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let at = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);

    // Both change the same note before either hears of the other's change. Ann's arrives
    // first, so Ben's wins.
    set_pitch(&mut ann, at, 62);
    until(&mut [&mut ann], "ann's edit is confirmed", |s| {
        s[0].unconfirmed() == 0
    });
    set_pitch(&mut ben, at, 65);
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ann.project(), at), Some(65));

    for session in [&ann, &ben] {
        let [entry] = session.displaced() else {
            panic!("{:?}", session.displaced())
        };
        assert_eq!(entry.by, ben.user());
        assert_eq!(entry.lost.lost_user, ann.user());
        assert_eq!(entry.lost.entity, EntityRef::Note(at.0, at.1, at.2));
        assert!(session.can_restore(&entry.lost));
    }

    let lost = ben.displaced()[0].lost.clone();
    assert!(ben.restore(&lost));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ann.project(), at), Some(62));
    assert!(
        !ann.can_restore(&lost),
        "it is back, so there is nothing to restore"
    );
    assert_eq!(ann.displaced().len(), 1, "restoring knew of the edit it replaced");

    // The restore is Ben's own edit and undoes like one.
    assert!(ben.undo());
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ann.project(), at), Some(65));
}

#[test]
fn edits_made_offline_to_the_same_thing_meet_when_the_server_is_back() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let addr: SocketAddr = server.local_addr().unwrap();
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let at = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);

    server.stop();
    until(
        &mut [&mut ann, &mut ben],
        "both notice the server is gone",
        |sessions| sessions.iter().all(|s| s.status() == Status::Reconnecting),
    );
    set_pitch(&mut ann, at, 62);
    set_pitch(&mut ben, at, 65);

    // Whoever gets through first loses; both are told the same story.
    let _server = serve(dir.path(), &addr.to_string());
    settle(&mut [&mut ann, &mut ben]);
    let [entry] = ann.displaced() else {
        panic!("{:?}", ann.displaced())
    };
    let [same] = ben.displaced() else {
        panic!("{:?}", ben.displaced())
    };
    assert_eq!(
        (entry.seq, entry.by, &entry.lost),
        (same.seq, same.by, &same.lost)
    );
    let (winner, loser) = if entry.by == ann.user() {
        (62, 65)
    } else {
        (65, 62)
    };
    assert_eq!(pitch(ann.project(), at), Some(winner));

    let lost = entry.lost.clone();
    assert!(ann.restore(&lost));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(loser));
    assert!(
        !ann.can_restore(&lost) && !ben.can_restore(&lost),
        "nothing left to restore"
    );

    // A restore is an edit of one's own: undo takes it back.
    assert!(ann.undo());
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(pitch(ben.project(), at), Some(winner));
}

#[test]
fn what_one_drags_is_locked_for_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let (track, clip, id) = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);
    let (the_clip, the_note) = (EntityRef::Clip(track, clip), EntityRef::Note(track, clip, id));
    assert_eq!(ben.locked_by(&the_clip), None);

    ann.set_editing(Some(the_clip));
    until(&mut [&mut ann, &mut ben], "ben sees the lock", |s| {
        s[1].locked_by(&the_clip).is_some()
    });
    assert_eq!(ben.locked_by(&the_clip), Some("ann"));
    assert_eq!(
        ben.locked_by(&the_note),
        Some("ann"),
        "what is inside the clip is locked with it"
    );
    assert_eq!(ben.locked_by(&EntityRef::Track(track)), None);
    assert_eq!(ann.locked_by(&the_clip), None, "one's own drag locks nothing");
    assert!(ben.next_frame_in().is_some());

    // The lock holds as long as the drag goes on, however long that is.
    let hold = Instant::now() + Duration::from_millis(2600);
    while Instant::now() < hold {
        ann.set_editing(Some(the_clip));
        ann.frame();
        ben.frame();
        assert_eq!(ben.locked_by(&the_clip), Some("ann"));
        std::thread::sleep(Duration::from_millis(20));
    }

    ann.set_editing(None);
    until(&mut [&mut ann, &mut ben], "the lock is released", |s| {
        s[1].locked_by(&the_clip).is_none()
    });

    // A lock does not outlive its holder.
    ann.set_editing(Some(the_clip));
    until(&mut [&mut ann, &mut ben], "ben sees the lock again", |s| {
        s[1].locked_by(&the_clip).is_some()
    });
    drop(ann);
    until(&mut [&mut ben], "the lock is gone with ann", |s| {
        s[0].locked_by(&the_clip).is_none()
    });
}

#[test]
fn joining_fails_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let engine = || Engine::manual(48_000, false);

    let mut wrong_token = server.join_info().clone();
    wrong_token.token = "0".repeat(32);
    let error = Session::join(wrong_token, who("eve"), engine(), Arc::new(|| {}))
        .err()
        .unwrap();
    assert!(error.to_string().contains("token"), "{error}");

    let mut wrong_server = server.join_info().clone();
    wrong_server.fingerprint[0] ^= 1;
    let started = Instant::now();
    let error = Session::join(wrong_server, who("eve"), engine(), Arc::new(|| {}))
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("does not match the join string"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a wrong pin is not worth retrying"
    );

    // The waker fires when something arrives.
    let (woken_tx, woken) = std::sync::mpsc::channel();
    let waker = Arc::new(move || {
        let _ = woken_tx.send(());
    });
    let mut ann = Session::join(server.join_info().clone(), who("ann"), engine(), waker).unwrap();
    while woken.try_recv().is_ok() {}
    let mut ben = join(&server, "ben");
    add_track(&mut ben, "Lead");
    woken
        .recv_timeout(Duration::from_secs(10))
        .expect("ann is woken for ben's edit");
    settle(&mut [&mut ann, &mut ben]);
}

#[test]
fn undo_takes_back_only_ones_own_edits_and_brings_back_what_it_removed() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (join(&server, "ann"), join(&server, "ben"));
    let (track, clip, _) = add_track(&mut ann, "Lead");
    settle(&mut [&mut ann, &mut ben]);

    // Ben moves the clip, Ann turns the track down; Ann's undo takes back hers only.
    assert!(ben.edit(vec![Op::ClipSet {
        track,
        id: clip,
        prop: ClipProp::Start(Tick::bars(1)),
    }]));
    assert!(ann.edit(vec![Op::TrackSetMix {
        track,
        prop: MixProp::Gain(0.25),
    }]));
    settle(&mut [&mut ann, &mut ben]);
    assert!(ann.undo());
    settle(&mut [&mut ann, &mut ben]);
    let lead = ben.project().track(track).unwrap().content.clone();
    assert_eq!(lead.gain, 1.0);
    assert_eq!(lead.clips[&clip].start, Tick::bars(1), "Ben's move stays");

    // Ann removes the track Ben made; her undo brings it back, clip and all, for both.
    let (theirs, their_clip, _) = add_track(&mut ben, "Bass");
    settle(&mut [&mut ann, &mut ben]);
    assert!(ann.edit(vec![Op::TrackRemove { id: theirs }]));
    settle(&mut [&mut ann, &mut ben]);
    assert!(ben.project().track(theirs).is_none());
    assert!(ann.undo());
    settle(&mut [&mut ann, &mut ben]);
    let back = ben.project().track(theirs).expect("the track is back");
    assert_eq!(back.shell.name, "Bass");
    assert_eq!(back.content.clips[&their_clip].notes.len(), 1);
}

#[test]
fn the_others_see_someone_leave() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, ben) = (join(&server, "ann"), join(&server, "ben"));
    until(&mut [&mut ann], "ann sees ben", |s| s[0].peers().len() == 2);
    drop(ben);
    until(&mut [&mut ann], "ann sees ben leave", |s| s[0].peers().len() == 1);
}

#[test]
fn joining_a_server_that_is_not_there_gives_up_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let info = server.join_info().clone();
    server.stop();
    let started = Instant::now();
    let error = Session::join(info, who("zed"), Engine::manual(48_000, false), Arc::new(|| {}))
        .err()
        .expect("nobody answers");
    assert!(error.to_string().contains("could not join"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_huge_first_message_is_turned_away_before_the_token_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let info = server.join_info().clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let answered = runtime.block_on(async move {
        let (_endpoint, connection) = mdaw_net::connect(&info).await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let hello = mdaw_proto::ClientMsg::Hello(mdaw_proto::Hello {
            proto_version: mdaw_proto::PROTO_VERSION,
            token: "wrong".into(),
            user_key: [1; 16],
            // A megabyte of name: far more than any hello needs.
            name: "x".repeat(1 << 20),
            resume: None,
        });
        let _ = mdaw_net::write_frame(&mut send, &hello).await;
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            mdaw_net::read_frame::<mdaw_proto::ServerMsg>(&mut recv),
        )
        .await;
        matches!(answer, Ok(Ok(Some(_))))
    });
    assert!(!answered, "no answer, the connection is just ended");
    // And the server goes on as before.
    let mut ann = join(&server, "ann");
    settle(&mut [&mut ann]);
}
