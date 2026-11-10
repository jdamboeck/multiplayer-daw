//! One editing session: the replica, the thing that orders its edits, and the audio engine.

use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::{
    ActorId, BuiltinKind, Device, DeviceKind, IdGen, Op, Project, TempoPoint, Tick, TrackContent, TrackId,
    TrackShell, UserId, apply_all,
};
use mdaw_proto::Txn;
use mdaw_sync::{Gesture, Loopback, Replica};

use crate::theme;

/// Actor that owns the IDs of the starter project, so they cannot collide with the user's.
const TEMPLATE_ACTOR: ActorId = ActorId(0);
const LOCAL_ACTOR: ActorId = ActorId(1);
const LOCAL_USER: UserId = UserId(1);

pub struct Session {
    replica: Replica,
    server: Loopback,
    engine: Engine,
    gesture_seq: u64,
}

impl Session {
    /// A local session on `project`. Edits are ordered by an in-process loopback.
    pub fn local(project: Project, engine: Engine) -> Self {
        let mut session = Self {
            replica: Replica::new(LOCAL_ACTOR, project, 0),
            server: Loopback::new(0, LOCAL_USER),
            engine,
            gesture_seq: 0,
        };
        session.engine.set_project(session.replica.view());
        session
    }

    /// A local session on a new project with one instrument track.
    pub fn starter(engine: Engine) -> Self {
        let mut ids = IdGen::new(TEMPLATE_ACTOR);
        let mut project = Project::default();
        let tempo = TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: mdaw_model::DEFAULT_MILLI_BPM,
        };
        apply_all(
            &mut project,
            &[
                Op::TempoPut {
                    id: ids.tempo(),
                    point: tempo,
                },
                new_track(&mut ids, 0, None),
            ],
        );
        Self::local(project, engine)
    }

    pub fn project(&self) -> &Project {
        self.replica.view()
    }

    pub fn ids(&mut self) -> &mut IdGen {
        self.replica.ids()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Applies one user gesture as one undo step. Returns false if it changed nothing.
    pub fn edit(&mut self, ops: Vec<Op>) -> bool {
        let txn = self.replica.submit(ops);
        self.send(txn)
    }

    /// Like [`Self::edit`], but edits with the same gesture collapse into one undo step. Use it
    /// for continuous controls, so the change is audible while the control moves.
    pub fn edit_gesture(&mut self, ops: Vec<Op>, gesture: Gesture) -> bool {
        let txn = self.replica.submit_gesture(ops, gesture);
        self.send(txn)
    }

    pub fn new_gesture(&mut self) -> Gesture {
        self.gesture_seq += 1;
        Gesture(self.gesture_seq)
    }

    pub fn can_undo(&self) -> bool {
        self.replica.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.replica.can_redo()
    }

    pub fn undo(&mut self) -> bool {
        let txn = self.replica.undo();
        self.send(txn)
    }

    pub fn redo(&mut self) -> bool {
        let txn = self.replica.redo();
        self.send(txn)
    }

    /// Call once per UI frame.
    pub fn frame(&mut self) {
        self.engine.set_project(self.replica.view());
        self.engine.tick();
    }

    fn send(&mut self, txn: Option<Txn>) -> bool {
        let Some(txn) = txn else { return false };
        let event = self.server.sequence(self.replica.actor(), txn);
        self.replica
            .on_event(&event)
            .expect("the loopback delivers events in order");
        self.engine.set_project(self.replica.view());
        true
    }
}

/// The op that adds an instrument track named after its position.
pub fn new_track(ids: &mut IdGen, index: usize, after: Option<TrackId>) -> Op {
    let mut content = TrackContent::default();
    let kind = DeviceKind::Builtin {
        kind: BuiltinKind::Pocket,
        dsp_version: pocket::DSP_VERSION,
    };
    content.devices.push_back(Device::new(ids.device(), kind));
    let shell = TrackShell {
        name: format!("Track {}", index + 1),
        color: theme::PALETTE[index % theme::PALETTE.len()],
    };
    Op::TrackAdd {
        id: ids.track(),
        shell,
        content,
        after,
    }
}
