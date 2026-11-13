//! Keeps the engine in step with the project by comparing two states of it.
//!
//! The application's view of the project changes in many ways: its own optimistic edits, events
//! from the server, a rebase, a rollback, a fresh snapshot. [`sync_project`] does not care which:
//! given the project the engine was last brought to (`before`) and the one it should hold now
//! (`after`), it makes the calls that turn one into the other, and no others.
//!
//! The model is built from `imbl` persistent collections and `Arc`s, so unchanged parts of two
//! states share memory. Whatever is shared is skipped without looking inside: a single changed
//! note costs a walk down to its clip and one `notes_put` call. Values are read from `after`, as
//! in [`crate::apply`], whose mapping (Pocket on 4osc, CLAP parameter ranges, what is reported
//! as a [`Problem`]) is the same here.

use crate::apply::{
    BlobResolver, Problem, Sync, effective_tempo, ordered_tracks, raw_clip, raw_device, raw_note, raw_tempo,
    raw_track,
};
use crate::{Engine, EntityKind, Error};
use imbl::ordmap::DiffItem;
use mdaw_model::{Clip, ClipId, Device, DeviceId, DeviceKind, Project, TrackContent, TrackId};
use std::collections::HashSet;
use std::sync::Arc;

/// Brings the engine from `before` (what it holds now, as last loaded or synced) to `after`.
///
/// `blobs` gives plugin states by hash. Returns what could not be carried over, as
/// [`crate::load_project`] does. `before` must be what the engine was last brought to: the diff
/// is only as right as that. After an engine restart, or when in doubt, use
/// [`crate::load_project`].
pub fn sync_project(
    engine: &Engine,
    before: &Project,
    after: &Project,
    blobs: BlobResolver<'_>,
) -> Result<Vec<Problem>, Error> {
    let mut sync = Sync::new(engine, blobs);
    if before.master_gain != after.master_gain {
        engine.master_set_gain(after.master_gain)?;
    }
    if !before.tempo.ptr_eq(&after.tempo) {
        tempo(&mut sync, before, after)?;
    }
    if !before.tracks.ptr_eq(&after.tracks) || !before.track_order.ptr_eq(&after.track_order) {
        tracks(&mut sync, before, after)?;
    }
    Ok(sync.problems)
}

/// The engine holds one point per tick, the winner of the model's points there.
fn tempo(sync: &mut Sync<'_>, before: &Project, after: &Project) -> Result<(), Error> {
    let (was, now) = (effective_tempo(before), effective_tempo(after));
    if was == now {
        return Ok(());
    }
    let winning: HashSet<_> = now.values().map(|(id, _)| *id).collect();
    for (id, _) in was.values() {
        if !winning.contains(id) {
            sync.engine.tempo_remove(raw_tempo(*id))?;
        }
    }
    for (tick, (id, milli_bpm)) in &now {
        if was.get(tick) != Some(&(*id, *milli_bpm)) {
            sync.put_tempo(*id, *tick, *milli_bpm)?;
        }
    }
    Ok(())
}

fn tracks(sync: &mut Sync<'_>, before: &Project, after: &Project) -> Result<(), Error> {
    let engine = sync.engine;
    let mut added = Vec::new();
    for item in before.tracks.diff(&after.tracks) {
        match item {
            DiffItem::Remove(id, _) => engine.track_remove(raw_track(*id))?,
            DiffItem::Add(id, _) => added.push(*id),
            DiffItem::Update {
                old: (id, old),
                new: (_, new),
            } => {
                if Arc::ptr_eq(old, new) {
                    continue;
                }
                if old.shell.name != new.shell.name {
                    engine.track_set_name(raw_track(*id), &new.shell.name)?;
                }
                if !Arc::ptr_eq(&old.content, &new.content) {
                    track_content(sync, *id, &old.content, &new.content)?;
                }
            }
        }
    }

    // The engine's order now: `before`'s without the removed tracks; new tracks go in behind
    // their left neighbour in `after`, and then everything is moved into `after`'s order.
    let want: Vec<TrackId> = ordered_tracks(after).into_iter().map(|(id, _)| id).collect();
    let mut have: Vec<TrackId> = ordered_tracks(before)
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| after.tracks.contains_key(id))
        .collect();
    let added: HashSet<TrackId> = added.into_iter().collect();
    for (index, id) in want.iter().enumerate() {
        if !added.contains(id) {
            continue;
        }
        let behind = index.checked_sub(1).map(|i| want[i]);
        sync.put_track(after, *id, behind.map_or(0, raw_track))?;
        let at = behind
            .and_then(|b| have.iter().position(|h| *h == b))
            .map_or(if behind.is_some() { have.len() } else { 0 }, |p| p + 1);
        have.insert(at, *id);
    }
    put_in_order(&mut have, &want, |id, behind| {
        engine.track_move(raw_track(id), behind.map_or(0, raw_track))
    })
}

/// Moves items of `have` until it is `want` (both hold the same items), one `move_to(item,
/// behind)` per item out of place.
fn put_in_order<T: Copy + PartialEq>(
    have: &mut Vec<T>,
    want: &[T],
    mut move_to: impl FnMut(T, Option<T>) -> Result<(), Error>,
) -> Result<(), Error> {
    for (index, item) in want.iter().enumerate() {
        if have.get(index) == Some(item) {
            continue;
        }
        let behind = index.checked_sub(1).map(|i| want[i]);
        move_to(*item, behind)?;
        if let Some(from) = have.iter().position(|h| h == item) {
            have.remove(from);
        }
        have.insert(index.min(have.len()), *item);
    }
    Ok(())
}

fn track_content(
    sync: &mut Sync<'_>,
    track: TrackId,
    old: &TrackContent,
    new: &TrackContent,
) -> Result<(), Error> {
    if (old.gain, old.pan, old.mute) != (new.gain, new.pan, new.mute) {
        sync.engine
            .track_set_mix(raw_track(track), new.gain, new.pan, new.mute)?;
    }
    if !old.devices.ptr_eq(&new.devices) && old.devices != new.devices {
        devices(sync, track, old, new)?;
    }
    if !old.clips.ptr_eq(&new.clips) {
        for item in old.clips.diff(&new.clips) {
            match item {
                DiffItem::Remove(id, _) => sync.engine.clip_remove(raw_clip(*id))?,
                DiffItem::Add(id, clip) => sync.put_clip(track, *id, clip)?,
                DiffItem::Update {
                    old: (id, old),
                    new: (_, new),
                } => {
                    if !Arc::ptr_eq(old, new) {
                        clip(sync, track, *id, old, new)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn clip(sync: &mut Sync<'_>, track: TrackId, id: ClipId, old: &Clip, new: &Clip) -> Result<(), Error> {
    if old.length.0 < 1 || new.length.0 < 1 {
        // Into or out of the engine's range: put whole (or removed, and reported).
        return sync.put_clip(track, id, new);
    }
    let raw = raw_clip(id);
    if (old.start, old.length) != (new.start, new.length) {
        sync.engine.clip_set_range(raw, new.start.0, new.length.0)?;
    }
    if old.name != new.name {
        sync.engine.clip_set_name(raw, &new.name)?;
    }
    if old.notes.ptr_eq(&new.notes) {
        return Ok(());
    }
    let mut put = Vec::new();
    let mut removed = Vec::new();
    for item in old.notes.diff(&new.notes) {
        match item {
            DiffItem::Remove(note, _) => removed.push(raw_note(*note)),
            DiffItem::Add(note, value)
            | DiffItem::Update {
                new: (note, value), ..
            } => put.push((*note, value)),
        }
    }
    if !removed.is_empty() {
        sync.engine.notes_remove(raw, &removed)?;
    }
    if !put.is_empty() {
        sync.put_notes(track, id, put)?;
    }
    Ok(())
}

/// Whether two devices are the same thing in the engine: the same built-in, or the same plugin.
fn same_kind(a: &Device, b: &Device) -> bool {
    match (&a.kind, &b.kind) {
        (DeviceKind::Builtin { kind: x, .. }, DeviceKind::Builtin { kind: y, .. }) => x == y,
        (
            DeviceKind::Plugin {
                format: fa, id: ia, ..
            },
            DeviceKind::Plugin {
                format: fb, id: ib, ..
            },
        ) => fa == fb && ia == ib,
        _ => false,
    }
}

fn devices(sync: &mut Sync<'_>, track: TrackId, old: &TrackContent, new: &TrackContent) -> Result<(), Error> {
    let engine = sync.engine;
    let find = |content: &TrackContent, id: DeviceId| content.devices.iter().find(|d| d.id == id).cloned();
    // Gone, or something else under the same id now (only a hand-made document has that).
    let mut renew = HashSet::new();
    for device in &old.devices {
        match find(new, device.id) {
            None => engine.device_remove(raw_device(device.id))?,
            Some(now) if !same_kind(device, &now) => {
                engine.device_remove(raw_device(device.id))?;
                renew.insert(device.id);
            }
            Some(_) => {}
        }
    }
    // New ones, in order, each behind its nearest left neighbour the engine has.
    let project_new = single_track(track, new);
    for device in &new.devices {
        if find(old, device.id).is_none() || renew.contains(&device.id) {
            let behind = sync.device_predecessor(&project_new, track, device.id);
            sync.put_device(track, device, behind)?;
        }
    }
    // Changed settings of those that stayed.
    for device in &new.devices {
        let Some(was) = find(old, device.id) else { continue };
        if renew.contains(&device.id)
            || was == *device
            || !engine.has(EntityKind::Device, raw_device(device.id))
        {
            continue;
        }
        if was.bypass != device.bypass {
            engine.device_set_bypass(raw_device(device.id), device.bypass)?;
        }
        match &device.kind {
            DeviceKind::Builtin { .. } => {
                if was.params != device.params {
                    sync.put_pocket(track, device)?;
                }
            }
            DeviceKind::Plugin { state, .. } => {
                let state_was = match &was.kind {
                    DeviceKind::Plugin { state, .. } => *state,
                    DeviceKind::Builtin { .. } => None,
                };
                if state_was != *state {
                    match state {
                        Some(blob) => sync.put_state(track, device.id, *blob),
                        None => engine.device_reset_state(raw_device(device.id))?,
                    }
                    // The parameter values go on top of the state, as on a load.
                    sync.put_params(track, device)?;
                } else if was.params != device.params {
                    let params = engine.device_params(raw_device(device.id))?;
                    for item in was.params.diff(&device.params) {
                        let (param, value) = match item {
                            DiffItem::Remove(param, _) => (*param, None),
                            DiffItem::Add(param, value)
                            | DiffItem::Update {
                                new: (param, value), ..
                            } => (*param, Some(*value)),
                        };
                        sync.put_param(track, device.id, &params, param, value)?;
                    }
                }
            }
        }
    }
    // Order of what the engine has.
    let in_engine = |content: &TrackContent| -> Vec<DeviceId> {
        content
            .devices
            .iter()
            .map(|d| d.id)
            .filter(|id| engine.has(EntityKind::Device, raw_device(*id)))
            .collect()
    };
    let want = in_engine(new);
    let wanted: HashSet<DeviceId> = want.iter().copied().collect();
    // Where they are: new ones were put in their place already; the others are where they were.
    let mut have: Vec<DeviceId> = Vec::new();
    for id in old
        .devices
        .iter()
        .map(|d| d.id)
        .filter(|id| wanted.contains(id) && !renew.contains(id))
    {
        have.push(id);
    }
    for (index, id) in want.iter().enumerate() {
        if have.contains(id) {
            continue;
        }
        let behind = index.checked_sub(1).map(|i| want[i]);
        let at = behind
            .and_then(|b| have.iter().position(|h| *h == b))
            .map_or(0, |p| p + 1);
        have.insert(at, *id);
    }
    put_in_order(&mut have, &want, |id, behind| {
        engine.device_move(raw_device(id), behind.map_or(0, raw_device))
    })
}

/// A project of one track, for the helpers that look a track's devices up in a project.
fn single_track(track: TrackId, content: &TrackContent) -> Project {
    let mut project = Project::default();
    project.tracks.insert(
        track,
        Arc::new(mdaw_model::Track {
            shell: mdaw_model::TrackShell {
                name: String::new(),
                color: 0,
            },
            content: Arc::new(content.clone()),
        }),
    );
    project.track_order.push_back(track);
    project
}
