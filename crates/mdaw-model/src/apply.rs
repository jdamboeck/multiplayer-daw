use crate::doc::*;
use crate::ids::*;
use crate::op::*;
use imbl::Vector;
use std::sync::Arc;

/// Applies one op. Returns the op that undoes it, or `None` when the op changed nothing
/// (missing target, duplicate ID, or the property already had that value).
///
/// Total and deterministic: never panics, and depends only on its arguments.
pub fn apply(project: &mut Project, op: &Op) -> Option<Op> {
    match op {
        Op::ProjectSetName { name } => {
            let old = set(&mut project.name, name.clone())?;
            Some(Op::ProjectSetName { name: old })
        }
        Op::MasterSetGain { gain } => {
            let old = set(&mut project.master_gain, *gain)?;
            Some(Op::MasterSetGain { gain: old })
        }
        Op::TempoPut { id, point } => {
            if project.tempo.get(id) == Some(point) {
                return None;
            }
            Some(match project.tempo.insert(*id, *point) {
                Some(old) => Op::TempoPut { id: *id, point: old },
                None => Op::TempoRemove { id: *id },
            })
        }
        Op::TempoRemove { id } => {
            let old = project.tempo.remove(id)?;
            Some(Op::TempoPut { id: *id, point: old })
        }
        Op::TrackAdd {
            id,
            shell,
            content,
            after,
        } => {
            if project.tracks.contains_key(id) {
                return None;
            }
            project
                .tracks
                .insert(*id, Arc::new(Track::new(shell.clone(), content.clone())));
            // A stale entry can only exist if the document was built by hand; keep it unique.
            project.track_order.retain(|t| t != id);
            insert_after(
                &mut project.track_order,
                *id,
                after.map(|a| move |t: &TrackId| *t == a),
            );
            Some(Op::TrackRemove { id: *id })
        }
        Op::TrackMove { id, after } => {
            if !project.tracks.contains_key(id) || *after == Some(*id) {
                return None;
            }
            let before = predecessor(&project.track_order, |t| t == id)?;
            let before = before.copied();
            if before == *after {
                return None;
            }
            let old_order = project.track_order.clone();
            project.track_order.retain(|t| t != id);
            insert_after(
                &mut project.track_order,
                *id,
                after.map(|a| move |t: &TrackId| *t == a),
            );
            if project.track_order == old_order {
                return None;
            }
            Some(Op::TrackMove {
                id: *id,
                after: before,
            })
        }
        Op::TrackSetShell { id, prop } => {
            let track = project.tracks.get(id)?;
            let old = match prop {
                ShellProp::Name(name) if *name != track.shell.name => {
                    ShellProp::Name(track.shell.name.clone())
                }
                ShellProp::Color(color) if *color != track.shell.color => ShellProp::Color(track.shell.color),
                _ => return None,
            };
            let shell = &mut Arc::make_mut(project.tracks.get_mut(id)?).shell;
            match prop {
                ShellProp::Name(name) => shell.name = name.clone(),
                ShellProp::Color(color) => shell.color = *color,
            }
            Some(Op::TrackSetShell { id: *id, prop: old })
        }
        Op::TrackRemove { id } => {
            let track = project.tracks.remove(id)?;
            let after = predecessor(&project.track_order, |t| t == id).flatten().copied();
            project.track_order.retain(|t| t != id);
            Some(Op::TrackAdd {
                id: *id,
                shell: track.shell.clone(),
                content: (*track.content).clone(),
                after,
            })
        }
        Op::TrackSetMix { track, prop } => {
            let content = project.tracks.get(track)?.content.as_ref();
            let old = match *prop {
                MixProp::Gain(v) if v != content.gain => MixProp::Gain(content.gain),
                MixProp::Pan(v) if v != content.pan => MixProp::Pan(content.pan),
                MixProp::Mute(v) if v != content.mute => MixProp::Mute(content.mute),
                _ => return None,
            };
            let content = content_mut(project, *track)?;
            match *prop {
                MixProp::Gain(v) => content.gain = v,
                MixProp::Pan(v) => content.pan = v,
                MixProp::Mute(v) => content.mute = v,
            }
            Some(Op::TrackSetMix {
                track: *track,
                prop: old,
            })
        }
        Op::ClipAdd { track, id, clip } => {
            if project.tracks.get(track)?.content.clips.contains_key(id) {
                return None;
            }
            content_mut(project, *track)?
                .clips
                .insert(*id, Arc::new(clip.clone()));
            Some(Op::ClipRemove {
                track: *track,
                id: *id,
            })
        }
        Op::ClipRemove { track, id } => {
            if !project.tracks.get(track)?.content.clips.contains_key(id) {
                return None;
            }
            let clip = content_mut(project, *track)?.clips.remove(id)?;
            Some(Op::ClipAdd {
                track: *track,
                id: *id,
                clip: (*clip).clone(),
            })
        }
        Op::ClipSet { track, id, prop } => {
            let clip = project.tracks.get(track)?.content.clips.get(id)?;
            let old = match prop {
                ClipProp::Start(v) if *v != clip.start => ClipProp::Start(clip.start),
                ClipProp::Length(v) if *v != clip.length => ClipProp::Length(clip.length),
                ClipProp::Name(v) if *v != clip.name => ClipProp::Name(clip.name.clone()),
                _ => return None,
            };
            let clip = clip_mut(project, *track, *id)?;
            match prop {
                ClipProp::Start(v) => clip.start = *v,
                ClipProp::Length(v) => clip.length = *v,
                ClipProp::Name(v) => clip.name = v.clone(),
            }
            Some(Op::ClipSet {
                track: *track,
                id: *id,
                prop: old,
            })
        }
        Op::NoteAdd {
            track,
            clip,
            id,
            note,
        } => {
            if project
                .tracks
                .get(track)?
                .content
                .clips
                .get(clip)?
                .notes
                .contains_key(id)
            {
                return None;
            }
            clip_mut(project, *track, *clip)?.notes.insert(*id, *note);
            Some(Op::NoteRemove {
                track: *track,
                clip: *clip,
                id: *id,
            })
        }
        Op::NoteRemove { track, clip, id } => {
            if !project
                .tracks
                .get(track)?
                .content
                .clips
                .get(clip)?
                .notes
                .contains_key(id)
            {
                return None;
            }
            let note = clip_mut(project, *track, *clip)?.notes.remove(id)?;
            Some(Op::NoteAdd {
                track: *track,
                clip: *clip,
                id: *id,
                note,
            })
        }
        Op::NoteSet {
            track,
            clip,
            id,
            prop,
        } => {
            let note = project
                .tracks
                .get(track)?
                .content
                .clips
                .get(clip)?
                .notes
                .get(id)?;
            let old = match *prop {
                NoteProp::Start(v) if v != note.start => NoteProp::Start(note.start),
                NoteProp::Dur(v) if v != note.dur => NoteProp::Dur(note.dur),
                NoteProp::Pitch(v) if v != note.pitch => NoteProp::Pitch(note.pitch),
                NoteProp::Vel(v) if v != note.vel => NoteProp::Vel(note.vel),
                _ => return None,
            };
            let note = clip_mut(project, *track, *clip)?.notes.get_mut(id)?;
            match *prop {
                NoteProp::Start(v) => note.start = v,
                NoteProp::Dur(v) => note.dur = v,
                NoteProp::Pitch(v) => note.pitch = v,
                NoteProp::Vel(v) => note.vel = v,
            }
            Some(Op::NoteSet {
                track: *track,
                clip: *clip,
                id: *id,
                prop: old,
            })
        }
        Op::DeviceAdd { track, device, after } => {
            if project.tracks.get(track)?.content.device(device.id).is_some() {
                return None;
            }
            let devices = &mut content_mut(project, *track)?.devices;
            insert_after(
                devices,
                device.clone(),
                after.map(|a| move |d: &Device| d.id == a),
            );
            Some(Op::DeviceRemove {
                track: *track,
                id: device.id,
            })
        }
        Op::DeviceRemove { track, id } => {
            let devices = &project.tracks.get(track)?.content.devices;
            let index = devices.iter().position(|d| d.id == *id)?;
            let after = index.checked_sub(1).and_then(|i| devices.get(i)).map(|d| d.id);
            let device = content_mut(project, *track)?.devices.remove(index);
            Some(Op::DeviceAdd {
                track: *track,
                device,
                after,
            })
        }
        Op::DeviceMove { track, id, after } => {
            if *after == Some(*id) {
                return None;
            }
            let devices = &project.tracks.get(track)?.content.devices;
            let index = devices.iter().position(|d| d.id == *id)?;
            let before = index.checked_sub(1).and_then(|i| devices.get(i)).map(|d| d.id);
            if before == *after {
                return None;
            }
            let mut moved = devices.clone();
            let device = moved.remove(index);
            insert_after(&mut moved, device, after.map(|a| move |d: &Device| d.id == a));
            if moved == *devices {
                return None;
            }
            content_mut(project, *track)?.devices = moved;
            Some(Op::DeviceMove {
                track: *track,
                id: *id,
                after: before,
            })
        }
        Op::DeviceSetParam {
            track,
            id,
            param,
            value,
        } => {
            let old = project
                .tracks
                .get(track)?
                .content
                .device(*id)?
                .params
                .get(param)
                .copied();
            if old == *value {
                return None;
            }
            let device = device_mut(project, *track, *id)?;
            match value {
                Some(v) => device.params.insert(*param, *v),
                None => device.params.remove(param),
            };
            Some(Op::DeviceSetParam {
                track: *track,
                id: *id,
                param: *param,
                value: old,
            })
        }
        Op::DeviceSetBypass { track, id, bypass } => {
            if project.tracks.get(track)?.content.device(*id)?.bypass == *bypass {
                return None;
            }
            device_mut(project, *track, *id)?.bypass = *bypass;
            Some(Op::DeviceSetBypass {
                track: *track,
                id: *id,
                bypass: !*bypass,
            })
        }
        Op::DeviceSetState { track, id, state } => {
            let old = match &project.tracks.get(track)?.content.device(*id)?.kind {
                DeviceKind::Clap { state: old, .. } if old != state => *old,
                _ => return None,
            };
            if let DeviceKind::Clap { state: slot, .. } = &mut device_mut(project, *track, *id)?.kind {
                *slot = *state;
            }
            Some(Op::DeviceSetState {
                track: *track,
                id: *id,
                state: old,
            })
        }
    }
}

/// Applies ops in order. Returns the ops that undo the whole batch, already in the order in
/// which they must be applied.
pub fn apply_all<'a>(project: &mut Project, ops: impl IntoIterator<Item = &'a Op>) -> Vec<Op> {
    let mut inverses: Vec<Op> = ops.into_iter().filter_map(|op| apply(project, op)).collect();
    inverses.reverse();
    inverses
}

fn set<T: PartialEq>(slot: &mut T, value: T) -> Option<T> {
    if *slot == value {
        None
    } else {
        Some(std::mem::replace(slot, value))
    }
}

/// Inserts behind the anchor. No anchor puts the item first; an anchor that is gone puts it last.
fn insert_after<T: Clone>(list: &mut Vector<T>, item: T, anchor: Option<impl Fn(&T) -> bool>) {
    let index = match anchor {
        None => 0,
        Some(is_anchor) => list.iter().position(is_anchor).map_or(list.len(), |i| i + 1),
    };
    list.insert(index, item);
}

/// `None` if no element matches, `Some(None)` if the match is first, else its left neighbour.
fn predecessor<T: Clone>(list: &Vector<T>, is_item: impl Fn(&T) -> bool) -> Option<Option<&T>> {
    let index = list.iter().position(is_item)?;
    Some(index.checked_sub(1).and_then(|i| list.get(i)))
}

fn content_mut(project: &mut Project, track: TrackId) -> Option<&mut TrackContent> {
    let track = Arc::make_mut(project.tracks.get_mut(&track)?);
    Some(Arc::make_mut(&mut track.content))
}

fn clip_mut(project: &mut Project, track: TrackId, clip: ClipId) -> Option<&mut Clip> {
    Some(Arc::make_mut(content_mut(project, track)?.clips.get_mut(&clip)?))
}

fn device_mut(project: &mut Project, track: TrackId, device: DeviceId) -> Option<&mut Device> {
    content_mut(project, track)?
        .devices
        .iter_mut()
        .find(|d| d.id == device)
}
