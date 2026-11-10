//! The M4 demo as a test: checkout, commit with samples, a tempo change during a checkout, and
//! a forced release, between two clients and a real server in one process.

use mdaw_client::checkout::Files;
use mdaw_client::{CommitState, Logged, Session, Status, Who};
use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::*;
use mdaw_proto::SampleKind;
use mdaw_server::{Config, Server};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn settle(sessions: &mut [&mut Session]) {
    until(sessions, "the clients agree", |sessions| {
        let head = sessions.iter().map(|s| s.seq()).max().unwrap();
        sessions
            .iter()
            .all(|s| s.unconfirmed() == 0 && s.seq() == head && s.status() == Status::Online)
    });
}

fn note(n: u64, ids: &mut IdGen, track: TrackId, clip: ClipId) -> Op {
    Op::NoteAdd {
        track,
        clip,
        id: ids.note(),
        note: Note {
            start: Tick::beats(n as i64),
            dur: Tick::beats(1),
            pitch: 60 + n as u8,
            vel: 100,
        },
    }
}

/// A Pocket track with a one-bar clip holding one note.
fn add_track(session: &mut Session) -> (TrackId, ClipId) {
    let ids = session.ids();
    let (track, clip, device) = (ids.track(), ids.clip(), ids.device());
    let mut content = TrackContent::default();
    let kind = DeviceKind::Builtin {
        kind: BuiltinKind::Pocket,
        dsp_version: pocket::DSP_VERSION,
    };
    content.devices.push_back(Device::new(device, kind));
    let first = note(0, session.ids(), track, clip);
    assert!(session.edit(vec![
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "keys".into(),
                color: 0x4FA3E0,
            },
            content,
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
        first,
    ]));
    (track, clip)
}

fn notes(session: &Session, track: TrackId, clip: ClipId) -> usize {
    session.project().track(track).unwrap().content.clips[&clip]
        .notes
        .len()
}

fn tempo(session: &Session) -> u32 {
    session.project().tempo_map().milli_bpm_at(Tick::ZERO)
}

fn set_tempo(session: &mut Session, milli_bpm: u32) {
    let id = *session.project().tempo.keys().next().unwrap();
    let point = TempoPoint {
        tick: Tick::ZERO,
        milli_bpm,
    };
    assert!(session.edit(vec![Op::TempoPut { id, point }]));
}

/// Checks out `track` for `owner` and waits until `other` sees the lock.
fn check_out(owner: &mut Session, other: &mut Session, track: TrackId) {
    owner.check_out(track);
    let user = owner.user();
    until(&mut [owner, other], "the checkout is everywhere", |s| {
        s[0].workspace(track).is_some() && s[1].checkout_owner(track) == Some(user)
    });
}

/// Renders the samples and waits for the dialog's choice to be possible.
fn prepare_commit(owner: &mut Session, other: &mut Session, track: TrackId) {
    assert!(owner.start_commit(track));
    until(&mut [owner, other], "the samples are rendered", |s| {
        matches!(s[0].commit_state(), Some(CommitState::Ready { .. }))
    });
}

#[test]
fn a_checkout_is_private_until_it_is_committed_with_samples() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(Config {
        data_dir: dir.path().join("server"),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    })
    .unwrap();
    let mut ann = join(&server, "ann");
    let mut ben = join(&server, "ben");
    let files = Files::new(dir.path().join("ann"));
    ann.set_files(files.clone());
    let (track, clip) = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);

    check_out(&mut ann, &mut ben, track);
    assert!(ben.locked(track).is_some_and(|why| why.contains("ann")));
    let edit = note(1, ben.ids(), track, clip);
    assert!(!ben.edit(vec![edit]), "Ben cannot touch Ann's checkout");

    let edits = vec![note(1, ann.ids(), track, clip), note(2, ann.ids(), track, clip)];
    assert!(ann.edit(edits));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(notes(&ann, track, clip), 3);
    assert_eq!(notes(&ben, track, clip), 1, "Ben hears the old content");
    // The work is on disk in case Ann's client goes down.
    let (journal, _) = files.load_journal(track).unwrap();
    assert_eq!(journal.len(), 2);

    prepare_commit(&mut ann, &mut ben, track);
    ann.finish_commit("two more notes", false);
    until(&mut [&mut ann, &mut ben], "Ben has the commit", |s| {
        s[1].checkout_owner(track).is_none() && notes(s[1], track, clip) == 3
    });
    assert!(
        files.load_journal(track).is_none(),
        "the journal is gone with the checkout"
    );

    let entry = ben.log().last().unwrap().clone();
    let Logged::Commit { summary, .. } = &entry.what else {
        panic!("expected a commit, got {entry:?}")
    };
    assert_eq!(summary, "two more notes");
    let samples = ben.samples(entry.seq).to_vec();
    let kinds: Vec<_> = samples.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [SampleKind::Solo, SampleKind::InContext]);
    assert_eq!(samples[0].range, TickRange::new(Tick::ZERO, Tick::bars(1)));

    // Ben downloads the sample and hears it.
    ben.play_sample(&samples[0]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        ben.frame();
        ben.engine_mut().pump(4800);
        if ben.sample_position(samples[0].blob).is_some_and(|at| at > 0.0) {
            break;
        }
        assert!(Instant::now() < deadline, "the sample never played");
        std::thread::sleep(Duration::from_millis(2));
    }
    let heard = ben.engine_mut().pump(4800).unwrap();
    assert!(heard.iter().any(|s| s.abs() > 0.01), "the sample has sound");

    // Live editing again.
    let edit = note(3, ben.ids(), track, clip);
    assert!(ben.edit(vec![edit]));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(notes(&ann, track, clip), 4);
}

#[test]
fn a_tempo_change_during_a_checkout_can_be_undone_by_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(Config {
        data_dir: dir.path().join("server"),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    })
    .unwrap();
    let mut ann = join(&server, "ann");
    let mut ben = join(&server, "ben");
    let (track, clip) = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    let old = tempo(&ann);

    check_out(&mut ann, &mut ben, track);
    let edit = note(1, ann.ids(), track, clip);
    ann.edit(vec![edit]);
    set_tempo(&mut ben, 150_000);
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(tempo(&ann), 150_000);

    prepare_commit(&mut ann, &mut ben, track);
    let Some(CommitState::Ready { plan }) = ann.commit_state() else {
        unreachable!()
    };
    assert!(plan.conflict);
    let kinds: Vec<_> = plan.to_listen().iter().map(|r| r.sample.kind).collect();
    // With one track, the track alone sounds like the project: each sound is offered once.
    assert_eq!(kinds, [SampleKind::ProjectBase, SampleKind::ProjectCurrent]);
    ann.finish_commit("", true);
    until(&mut [&mut ann, &mut ben], "the commit lands", |s| {
        s[1].checkout_owner(track).is_none()
    });
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(tempo(&ben), old, "the old tempo is back");
    assert_eq!(notes(&ben, track, clip), 2);
}

#[test]
fn a_forced_release_leaves_the_owner_a_fork() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(Config {
        data_dir: dir.path().join("server"),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    })
    .unwrap();
    let mut ann = join(&server, "ann");
    let mut ben = join(&server, "ben");
    let files = Files::new(dir.path().join("ann"));
    ann.set_files(files.clone());
    let (track, clip) = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);

    check_out(&mut ann, &mut ben, track);
    let edit = note(1, ann.ids(), track, clip);
    ann.edit(vec![edit]);
    ben.force_release(track);
    until(&mut [&mut ann, &mut ben], "the checkout is over", |s| {
        s[0].workspace(track).is_none() && s[1].checkout_owner(track).is_none()
    });
    assert_eq!(notes(&ann, track, clip), 1, "the track is as before");
    assert_eq!(ann.forks().len(), 1);
    assert_eq!(files.forks().len(), 1, "kept on disk");
    assert!(
        ann.notices()
            .iter()
            .any(|n| n.contains("ben ended your checkout"))
    );

    assert!(ann.restore_fork(0));
    settle(&mut [&mut ann, &mut ben]);
    assert_eq!(ben.project().tracks.len(), 2);
    let (_, restored) = ben
        .project()
        .ordered_tracks()
        .find(|(id, _)| *id != track)
        .unwrap();
    assert_eq!(restored.shell.name, "keys (fork)");
    assert_eq!(restored.content.clips[&clip].notes.len(), 2);
    assert!(ann.forks().is_empty() && files.forks().is_empty());
}

#[test]
fn the_work_of_a_checkout_survives_a_restart_of_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(Config {
        data_dir: dir.path().join("server"),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    })
    .unwrap();
    let mut ann = join(&server, "ann");
    let mut ben = join(&server, "ben");
    ann.set_files(Files::new(dir.path().join("ann")));
    let (track, clip) = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben]);
    check_out(&mut ann, &mut ben, track);
    let edit = note(1, ann.ids(), track, clip);
    ann.edit(vec![edit]);
    ann.frame();
    drop(ann);

    let mut ann = join(&server, "ann");
    ann.set_files(Files::new(dir.path().join("ann")));
    assert_eq!(notes(&ann, track, clip), 2, "the journal is back");
    assert!(ann.notices().iter().any(|n| n.contains("Picked up")));
    prepare_commit(&mut ann, &mut ben, track);
    ann.finish_commit("", false);
    until(&mut [&mut ann, &mut ben], "Ben has the commit", |s| {
        notes(s[1], track, clip) == 2
    });
}
