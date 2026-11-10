use mdaw_model::*;
use mdaw_proto::{Event, EventBody, SyncData, Txn};
use mdaw_sync::{Gesture, Loopback, Replica, SyncError};
use proptest::prelude::*;
use std::collections::VecDeque;

fn eid(n: u64) -> EntityId {
    EntityId(n)
}
const TRACK: fn() -> TrackId = || TrackId(eid(1));
const CLIP: fn() -> ClipId = || ClipId(eid(2));
const DEVICE: fn() -> DeviceId = || DeviceId(eid(3));
fn note_id(n: u64) -> NoteId {
    NoteId(eid(100 + n))
}

/// One track with a device and an empty four-bar clip.
fn base() -> Project {
    let mut project = Project::default();
    let ops = [
        Op::TrackAdd {
            id: TRACK(),
            shell: TrackShell {
                name: "one".into(),
                color: 1,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::DeviceAdd {
            track: TRACK(),
            device: Device::new(
                DEVICE(),
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    dsp_version: 1,
                },
            ),
            after: None,
        },
        Op::ClipAdd {
            track: TRACK(),
            id: CLIP(),
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(4),
                name: String::new(),
                notes: Default::default(),
            },
        },
    ];
    for op in &ops {
        assert!(apply(&mut project, op).is_some());
    }
    project
}

fn add(n: u64, pitch: u8) -> Op {
    let note = Note {
        start: Tick::beats(n as i64),
        dur: Tick::beats(1),
        pitch,
        vel: 100,
    };
    Op::NoteAdd {
        track: TRACK(),
        clip: CLIP(),
        id: note_id(n),
        note,
    }
}

fn pitch(n: u64, pitch: u8) -> Op {
    Op::NoteSet {
        track: TRACK(),
        clip: CLIP(),
        id: note_id(n),
        prop: NoteProp::Pitch(pitch),
    }
}

fn cutoff(value: f64) -> Op {
    Op::DeviceSetParam {
        track: TRACK(),
        id: DEVICE(),
        param: 11,
        value: Some(value),
    }
}

fn note_pitch(project: &Project, n: u64) -> Option<u8> {
    let clip = project.track(TRACK())?.content.clips.get(&CLIP())?;
    clip.notes.get(&note_id(n)).map(|note| note.pitch)
}

fn param(project: &Project) -> Option<f64> {
    project
        .track(TRACK())?
        .content
        .device(DEVICE())?
        .params
        .get(&11)
        .copied()
}

/// A server reduced to its ordering job.
struct Server {
    doc: Project,
    log: Vec<Event>,
}

impl Server {
    fn new() -> Self {
        Self {
            doc: base(),
            log: Vec::new(),
        }
    }

    fn sequence(&mut self, actor: ActorId, txn: Txn) -> Event {
        apply_all(&mut self.doc, &txn.ops);
        let event = Event {
            seq: self.log.len() as u64 + 1,
            actor,
            user: UserId(actor.0),
            wall_ts: 0,
            body: EventBody::Txn {
                client_txn_id: txn.client_txn_id,
                ops: txn.ops,
                superseded: vec![],
            },
        };
        self.log.push(event.clone());
        event
    }
}

/// A replica wired to a loopback: every transaction is confirmed at once.
struct Solo {
    replica: Replica,
    loopback: Loopback,
}

impl Solo {
    fn new() -> Self {
        Self {
            replica: Replica::new(ActorId(1), base(), 0),
            loopback: Loopback::new(0, UserId(1)),
        }
    }

    fn confirm(&mut self, txn: Option<Txn>) -> bool {
        let Some(txn) = txn else { return false };
        let event = self.loopback.sequence(self.replica.actor(), txn);
        self.replica.on_event(&event).unwrap();
        assert_eq!(self.replica.confirmed(), self.replica.view());
        true
    }

    fn submit(&mut self, ops: Vec<Op>) -> bool {
        let txn = self.replica.submit(ops);
        self.confirm(txn)
    }

    fn undo(&mut self) -> bool {
        let txn = self.replica.undo();
        self.confirm(txn)
    }

    fn redo(&mut self) -> bool {
        let txn = self.replica.redo();
        self.confirm(txn)
    }
}

#[test]
fn an_edit_shows_at_once_and_is_confirmed_later() {
    let mut server = Server::new();
    let mut replica = Replica::new(ActorId(1), base(), 0);
    let txn = replica.submit(vec![add(0, 60)]).unwrap();
    assert_eq!((txn.client_txn_id, txn.base_seq), (1, 0));
    assert_eq!(note_pitch(replica.view(), 0), Some(60));
    assert_eq!(note_pitch(replica.confirmed(), 0), None);
    assert_eq!(replica.pending().len(), 1);

    replica.on_event(&server.sequence(ActorId(1), txn)).unwrap();
    assert_eq!(replica.pending().len(), 0);
    assert_eq!(replica.seq(), 1);
    assert_eq!(replica.confirmed(), replica.view());
    assert_eq!(replica.view(), &server.doc);
}

#[test]
fn edits_that_change_nothing_are_not_sent() {
    let mut replica = Replica::new(ActorId(1), base(), 0);
    assert!(replica.submit(vec![]).is_none());
    assert!(replica.submit(vec![pitch(9, 60)]).is_none(), "no such note");
    let txn = replica
        .submit(vec![add(0, 60), add(0, 61), pitch(0, 60)])
        .unwrap();
    assert_eq!(txn.ops, vec![add(0, 60)], "only the op with an effect is kept");
    assert!(!replica.can_redo());
    replica.undo().unwrap();
    assert!(replica.undo().is_none());
}

#[test]
fn pending_edits_stay_on_top_of_what_others_do() {
    let mut server = Server::new();
    let mut a = Replica::new(ActorId(1), base(), 0);
    let mut b = Replica::new(ActorId(2), base(), 0);

    // Both write the same parameter; B's write reaches the server first, so A's wins.
    let from_a = a.submit(vec![cutoff(1000.0), add(0, 60)]).unwrap();
    let from_b = b.submit(vec![cutoff(2000.0), add(1, 62)]).unwrap();
    let first = server.sequence(ActorId(2), from_b);
    let second = server.sequence(ActorId(1), from_a);

    a.on_event(&first).unwrap();
    assert_eq!(
        param(a.view()),
        Some(1000.0),
        "A keeps seeing its own pending value"
    );
    assert_eq!(param(a.confirmed()), Some(2000.0));
    assert_eq!(note_pitch(a.view(), 1), Some(62), "and B's note as well");
    a.on_event(&second).unwrap();

    b.on_event(&first).unwrap();
    assert_eq!(param(b.view()), Some(2000.0));
    b.on_event(&second).unwrap();
    assert_eq!(param(b.view()), Some(1000.0), "the later write wins everywhere");

    assert_eq!(a.view(), &server.doc);
    assert_eq!(b.view(), &server.doc);
}

#[test]
fn a_rejected_edit_disappears() {
    let mut server = Server::new();
    let mut replica = Replica::new(ActorId(1), base(), 0);
    let first = replica.submit(vec![add(0, 60)]).unwrap();
    let second = replica.submit(vec![add(1, 62)]).unwrap();
    replica.on_rejected(first.client_txn_id);
    assert_eq!(note_pitch(replica.view(), 0), None);
    assert_eq!(note_pitch(replica.view(), 1), Some(62));
    replica.on_event(&server.sequence(ActorId(1), second)).unwrap();
    assert_eq!(replica.view(), &server.doc);
    // Undoing the rejected edit has nothing to take back; the older one is undone instead.
    replica.undo().unwrap();
    assert_eq!(note_pitch(replica.view(), 1), None);
    assert!(replica.undo().is_none());
}

#[test]
fn events_must_arrive_in_order() {
    let mut server = Server::new();
    let mut a = Replica::new(ActorId(1), base(), 0);
    let mut b = Replica::new(ActorId(2), base(), 0);
    let one = a.submit(vec![add(0, 60)]).unwrap();
    let two = a.submit(vec![add(1, 60)]).unwrap();
    let one = server.sequence(ActorId(1), one);
    let two = server.sequence(ActorId(1), two);
    assert_eq!(b.on_event(&two), Err(SyncError::Gap { expected: 1, got: 2 }));
    assert_eq!(b.view(), &base(), "a refused event changes nothing");
    b.on_event(&one).unwrap();
    assert_eq!(b.on_event(&one), Err(SyncError::Gap { expected: 2, got: 1 }));
    b.on_event(&two).unwrap();
    assert_eq!(b.view(), &server.doc);
}

#[test]
fn a_reconnect_continues_with_the_events_missed() {
    let mut server = Server::new();
    let mut replica = Replica::new(ActorId(1), base(), 0);
    let first = replica.submit(vec![add(0, 60)]).unwrap();
    let second = replica.submit(vec![add(1, 62)]).unwrap();
    let third = replica.submit(vec![pitch(1, 64)]).unwrap();

    // The server got the first two before the connection broke; someone else edited meanwhile.
    let missed = vec![
        server.sequence(ActorId(1), first),
        server.sequence(
            ActorId(2),
            Txn {
                client_txn_id: 1,
                base_seq: 0,
                ops: vec![add(5, 70)],
            },
        ),
        server.sequence(ActorId(1), second),
    ];
    let resend = replica
        .resync(ActorId(3), 3, SyncData::Events(missed), Some(2))
        .unwrap();
    assert_eq!(resend, std::slice::from_ref(&third));
    assert_eq!(replica.actor(), ActorId(3));
    assert_eq!(replica.seq(), 3);
    assert_eq!(replica.confirmed(), &server.doc);
    assert_eq!(
        note_pitch(replica.view(), 1),
        Some(64),
        "the unconfirmed edit stays on top"
    );
    assert_eq!(note_pitch(replica.view(), 5), Some(70));

    // The rest is confirmed under the new actor.
    replica.on_event(&server.sequence(ActorId(3), third)).unwrap();
    assert_eq!(replica.pending().len(), 0);
    assert_eq!(replica.view(), &server.doc);

    // IDs minted from now on belong to the new actor.
    assert_eq!(replica.ids().note().0.actor(), ActorId(3));
    // Undo still reaches back across the reconnect.
    let undone = replica.undo().unwrap();
    assert_eq!(undone.client_txn_id, 4);
    assert_eq!(note_pitch(replica.view(), 1), Some(62));
}

#[test]
fn a_reconnect_can_replace_the_document() {
    let mut replica = Replica::new(ActorId(1), base(), 0);
    let kept = replica.submit(vec![add(0, 60)]).unwrap();

    let mut current = base();
    apply(&mut current, &add(5, 70)).unwrap();
    let resend = replica
        .resync(ActorId(2), 40, SyncData::Snapshot(current.clone()), None)
        .unwrap();
    assert_eq!(resend, [kept]);
    assert_eq!(replica.seq(), 40);
    assert_eq!(replica.confirmed(), &current);
    assert_eq!(
        (note_pitch(replica.view(), 0), note_pitch(replica.view(), 5)),
        (Some(60), Some(70))
    );
}

#[test]
fn a_reconnect_with_a_hole_in_the_events_is_an_error() {
    let mut server = Server::new();
    let mut replica = Replica::new(ActorId(1), base(), 0);
    let txn = |n| Txn {
        client_txn_id: n,
        base_seq: 0,
        ops: vec![add(n, 60)],
    };
    let events: Vec<Event> = (1..=3).map(|n| server.sequence(ActorId(2), txn(n))).collect();

    let skipped = SyncData::Events(events[1..].to_vec());
    assert_eq!(
        replica.resync(ActorId(3), 3, skipped, None).err(),
        Some(SyncError::Gap { expected: 1, got: 2 })
    );
    let short = SyncData::Events(events[..2].to_vec());
    assert!(replica.resync(ActorId(3), 3, short, None).is_err());
    assert_eq!(
        (replica.seq(), replica.actor()),
        (0, ActorId(1)),
        "a refused resync changes nothing"
    );
    assert_eq!(replica.confirmed(), &base());
}

#[test]
fn undo_and_redo() {
    let mut solo = Solo::new();
    assert!(!solo.replica.can_undo());
    assert!(solo.submit(vec![add(0, 60), add(1, 64)]));
    assert!(solo.submit(vec![pitch(0, 72)]));
    assert!(solo.replica.can_undo() && !solo.replica.can_redo());

    assert!(solo.undo());
    assert_eq!(note_pitch(solo.replica.view(), 0), Some(60));
    assert!(solo.undo());
    assert_eq!(
        solo.replica.view(),
        &base(),
        "one undo takes back the whole gesture"
    );
    assert!(!solo.undo());

    assert!(solo.redo());
    assert_eq!(note_pitch(solo.replica.view(), 1), Some(64));
    assert!(solo.redo());
    assert_eq!(note_pitch(solo.replica.view(), 0), Some(72));
    assert!(!solo.redo());

    // A new edit after an undo discards what could have been redone.
    assert!(solo.undo());
    assert!(solo.submit(vec![pitch(1, 50)]));
    assert!(!solo.replica.can_redo());
    assert!(solo.undo());
    assert_eq!(note_pitch(solo.replica.view(), 1), Some(64));
}

#[test]
fn undo_restores_a_removed_clip_with_its_notes() {
    let mut solo = Solo::new();
    solo.submit(vec![add(0, 60), add(1, 64)]);
    let with_notes = solo.replica.view().clone();
    solo.submit(vec![Op::ClipRemove {
        track: TRACK(),
        id: CLIP(),
    }]);
    assert_eq!(note_pitch(solo.replica.view(), 0), None);
    solo.undo();
    assert_eq!(solo.replica.view(), &with_notes);
    solo.submit(vec![Op::TrackRemove { id: TRACK() }]);
    assert!(solo.replica.view().track(TRACK()).is_none());
    solo.undo();
    assert_eq!(solo.replica.view(), &with_notes);
}

#[test]
fn a_gesture_is_one_undo_step() {
    let mut solo = Solo::new();
    solo.submit(vec![cutoff(500.0)]);
    for step in 1..=40 {
        let txn = solo
            .replica
            .submit_gesture(vec![cutoff(500.0 + step as f64)], Gesture(7));
        assert!(
            solo.confirm(txn),
            "every step is sent, so others hear the knob move"
        );
    }
    assert_eq!(param(solo.replica.view()), Some(540.0));
    // A second drag of the same knob is its own step.
    let txn = solo.replica.submit_gesture(vec![cutoff(900.0)], Gesture(8));
    solo.confirm(txn);

    solo.undo();
    assert_eq!(param(solo.replica.view()), Some(540.0));
    solo.undo();
    assert_eq!(param(solo.replica.view()), Some(500.0));
    solo.redo();
    assert_eq!(param(solo.replica.view()), Some(540.0));
    solo.undo();
    solo.undo();
    assert_eq!(param(solo.replica.view()), None);
    assert!(!solo.undo());
}

#[test]
fn a_gesture_does_not_swallow_other_edits() {
    let mut solo = Solo::new();
    let txn = solo.replica.submit_gesture(vec![cutoff(100.0)], Gesture(1));
    solo.confirm(txn);
    solo.submit(vec![add(0, 60)]);
    // Same gesture id, but something else happened in between.
    let txn = solo.replica.submit_gesture(vec![cutoff(200.0)], Gesture(1));
    solo.confirm(txn);
    solo.undo();
    assert_eq!(param(solo.replica.view()), Some(100.0));
    assert_eq!(note_pitch(solo.replica.view(), 0), Some(60));
}

#[test]
fn undo_leaves_what_others_have_overwritten() {
    let mut server = Server::new();
    let mut a = Replica::new(ActorId(1), base(), 0);
    let mut b = Replica::new(ActorId(2), base(), 0);
    let deliver = |server: &mut Server, a: &mut Replica, b: &mut Replica, actor: u32, txn: Txn| {
        let event = server.sequence(ActorId(actor), txn);
        a.on_event(&event).unwrap();
        b.on_event(&event).unwrap();
    };

    // A adds a note and moves it, in one gesture, and sets the cutoff.
    let txn = a.submit(vec![add(0, 60), cutoff(1000.0)]).unwrap();
    deliver(&mut server, &mut a, &mut b, 1, txn);
    // B then changes the cutoff.
    let txn = b.submit(vec![cutoff(3000.0)]).unwrap();
    deliver(&mut server, &mut a, &mut b, 2, txn);

    // A's undo removes its note but must not touch B's cutoff.
    let txn = a.undo().unwrap();
    assert_eq!(txn.ops.len(), 1);
    deliver(&mut server, &mut a, &mut b, 1, txn);
    assert_eq!(note_pitch(&server.doc, 0), None);
    assert_eq!(param(&server.doc), Some(3000.0));

    // B's undo takes back only B's edit, and A's redo brings the note back.
    let txn = b.undo().unwrap();
    deliver(&mut server, &mut a, &mut b, 2, txn);
    assert_eq!(param(&server.doc), Some(1000.0));
    let txn = a.redo().unwrap();
    deliver(&mut server, &mut a, &mut b, 1, txn);
    assert_eq!(note_pitch(&server.doc, 0), Some(60));
    assert_eq!(a.view(), &server.doc);
    assert_eq!(b.view(), &server.doc);

    // An undo whose every part was overwritten does nothing and is skipped.
    let txn = a.submit(vec![pitch(0, 61)]).unwrap();
    deliver(&mut server, &mut a, &mut b, 1, txn);
    let txn = b.submit(vec![pitch(0, 70)]).unwrap();
    deliver(&mut server, &mut a, &mut b, 2, txn);
    let txn = a.undo().expect("falls through to the older step");
    deliver(&mut server, &mut a, &mut b, 1, txn);
    assert_eq!(
        note_pitch(&server.doc, 0),
        None,
        "the note A added earlier is removed"
    );
}

// ---- properties ----

fn any_op() -> impl Strategy<Value = Op> {
    let n = 0u64..4;
    let tick = (0i64..16).prop_map(Tick::beats);
    prop_oneof![
        6 => (n.clone(), 60u8..64).prop_map(|(n, p)| add(n, p)),
        3 => n.clone().prop_map(|n| Op::NoteRemove { track: TRACK(), clip: CLIP(), id: note_id(n) }),
        3 => (n.clone(), 60u8..64).prop_map(|(n, p)| pitch(n, p)),
        2 => (n.clone(), tick.clone()).prop_map(|(n, t)| Op::NoteSet {
            track: TRACK(), clip: CLIP(), id: note_id(n), prop: NoteProp::Start(t)
        }),
        2 => tick.clone().prop_map(|t| Op::ClipSet { track: TRACK(), id: CLIP(), prop: ClipProp::Start(t) }),
        2 => (0u8..4).prop_map(|v| cutoff(500.0 * v as f64)),
        1 => Just(Op::DeviceSetParam { track: TRACK(), id: DEVICE(), param: 11, value: None }),
        1 => (0u8..3).prop_map(|g| Op::TrackSetMix { track: TRACK(), prop: MixProp::Gain(g as f32 * 0.5) }),
        1 => (0u8..3).prop_map(|g| Op::MasterSetGain { gain: g as f32 * 0.5 }),
        1 => Just(Op::ClipRemove { track: TRACK(), id: CLIP() }),
        1 => Just(Op::ClipAdd {
            track: TRACK(),
            id: CLIP(),
            clip: Clip { start: Tick::ZERO, length: Tick::bars(2), name: "new".into(), notes: Default::default() },
        }),
        1 => Just(Op::TrackRemove { id: TRACK() }),
        1 => Just(Op::TrackAdd {
            id: TRACK(),
            shell: TrackShell { name: "again".into(), color: 2 },
            content: TrackContent::default(),
            after: None,
        }),
    ]
}

#[derive(Clone, Debug)]
enum Step {
    Submit(usize, Vec<Op>),
    Undo(usize),
    Redo(usize),
    /// The server takes the next transaction this replica sent.
    Sequence(usize),
    /// This replica receives the next event.
    Deliver(usize),
    /// The server refuses the next transaction this replica sent.
    Reject(usize),
}

fn step(replicas: usize) -> impl Strategy<Value = Step> {
    let who = 0..replicas;
    prop_oneof![
        5 => (who.clone(), prop::collection::vec(any_op(), 1..4)).prop_map(|(w, ops)| Step::Submit(w, ops)),
        2 => who.clone().prop_map(Step::Undo),
        1 => who.clone().prop_map(Step::Redo),
        5 => who.clone().prop_map(Step::Sequence),
        5 => who.clone().prop_map(Step::Deliver),
        1 => who.prop_map(Step::Reject),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Whatever the interleaving of edits, undos, rejections and deliveries: once everything
    /// has been delivered, every replica shows the server's document, and until then every
    /// view is the confirmed state plus that replica's own pending edits.
    #[test]
    fn replicas_converge(steps in prop::collection::vec(step(3), 1..80)) {
        let mut server = Server::new();
        let mut replicas: Vec<Replica> = (0..3).map(|i| Replica::new(ActorId(i + 1), base(), 0)).collect();
        let mut outbox: Vec<VecDeque<Txn>> = vec![VecDeque::new(); 3];
        let mut delivered = [0usize; 3];

        let check = |replica: &Replica| {
            let mut expected = replica.confirmed().clone();
            for txn in replica.pending() {
                apply_all(&mut expected, &txn.ops);
            }
            assert_eq!(replica.view(), &expected);
        };

        for step in steps {
            match step {
                Step::Submit(w, ops) => outbox[w].extend(replicas[w].submit(ops)),
                Step::Undo(w) => outbox[w].extend(replicas[w].undo()),
                Step::Redo(w) => outbox[w].extend(replicas[w].redo()),
                Step::Sequence(w) => {
                    if let Some(txn) = outbox[w].pop_front() {
                        prop_assert!(txn.base_seq <= server.log.len() as u64);
                        server.sequence(ActorId(w as u32 + 1), txn);
                    }
                }
                Step::Deliver(w) => {
                    if let Some(event) = server.log.get(delivered[w]) {
                        replicas[w].on_event(event).unwrap();
                        delivered[w] += 1;
                    }
                }
                Step::Reject(w) => {
                    // A rejection travels in order with the events: deliver those first.
                    if let Some(txn) = outbox[w].pop_front() {
                        for event in &server.log[delivered[w]..] {
                            replicas[w].on_event(event).unwrap();
                        }
                        delivered[w] = server.log.len();
                        replicas[w].on_rejected(txn.client_txn_id);
                    }
                }
            }
            replicas.iter().for_each(check);
        }

        for (w, queue) in outbox.iter_mut().enumerate() {
            while let Some(txn) = queue.pop_front() {
                server.sequence(ActorId(w as u32 + 1), txn);
            }
        }
        for (w, replica) in replicas.iter_mut().enumerate() {
            for event in &server.log[delivered[w]..] {
                replica.on_event(event).unwrap();
            }
            prop_assert_eq!(replica.pending().len(), 0);
            prop_assert_eq!(replica.view(), &server.doc);
            prop_assert_eq!(replica.confirmed(), &server.doc);
            prop_assert_eq!(replica.seq(), server.log.len() as u64);
        }
    }

    /// Working alone, undoing everything leads back to the start through every state passed on
    /// the way, and redoing everything leads forward again.
    #[test]
    fn undo_walks_back_through_history(
        edits in prop::collection::vec(prop::collection::vec(any_op(), 1..4), 1..40),
        lag in any::<bool>(),
    ) {
        let mut replica = Replica::new(ActorId(1), base(), 0);
        let mut loopback = Loopback::new(0, UserId(1));
        let mut wire = VecDeque::new();
        let mut settle = |replica: &mut Replica, wire: &mut VecDeque<Txn>, txn: Option<Txn>| {
            wire.extend(txn);
            // With lag, confirmations trail one transaction behind the edits.
            while wire.len() > lag as usize {
                let event = loopback.sequence(replica.actor(), wire.pop_front().unwrap());
                replica.on_event(&event).unwrap();
            }
        };

        let mut history = vec![replica.view().clone()];
        for ops in edits {
            let txn = replica.submit(ops);
            if txn.is_some() {
                settle(&mut replica, &mut wire, txn);
                history.push(replica.view().clone());
            }
        }
        for expected in history.iter().rev().skip(1) {
            let txn = replica.undo();
            prop_assert!(txn.is_some());
            settle(&mut replica, &mut wire, txn);
            prop_assert_eq!(replica.view(), expected);
        }
        prop_assert!(replica.undo().is_none());
        for expected in history.iter().skip(1) {
            let txn = replica.redo();
            prop_assert!(txn.is_some());
            settle(&mut replica, &mut wire, txn);
            prop_assert_eq!(replica.view(), expected);
        }
        prop_assert!(replica.redo().is_none());
    }
}
