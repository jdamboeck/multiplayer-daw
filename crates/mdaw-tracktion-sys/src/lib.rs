//! Raw FFI to the Tracktion Engine bridge: a hand-written mirror of
//! `native/tracktion-bridge/include/mdaw_engine.h`. The header's comments define the semantics
//! (threads, buffers, identity, idempotency); they are not repeated here. Use the safe wrapper in
//! `mdaw-tracktion` instead of calling these directly.
//!
//! The native library is built by `build.rs` with CMake (see docs/tracktion-integration.md).

#![allow(non_camel_case_types)]

use std::ffi::c_char;

pub const MDAW_ABI_VERSION: u32 = 1;
pub const MDAW_TICKS_PER_QUARTER: i64 = 960;

/// Opaque engine handle.
#[repr(C)]
pub struct mdaw_engine {
    _private: [u8; 0],
    // Not Send, not Sync, not Unpin: only ever used behind a pointer.
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

/// `mdaw_status`. A C enum is `int` here; returned as a plain integer so that an unknown value
/// from a newer library is not undefined behaviour.
pub type mdaw_status = i32;
pub const MDAW_OK: mdaw_status = 0;
pub const MDAW_ERR_ARGUMENT: mdaw_status = 1;
pub const MDAW_ERR_NOT_FOUND: mdaw_status = 2;
pub const MDAW_ERR_DEVICE: mdaw_status = 3;
pub const MDAW_ERR_PLUGIN: mdaw_status = 4;
pub const MDAW_ERR_IO: mdaw_status = 5;
pub const MDAW_ERR_INTERNAL: mdaw_status = 6;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct mdaw_engine_config {
    pub abi_version: u32,
    pub app_name: *const c_char,
    pub settings_dir: *const c_char,
    pub open_audio_device: i32,
    pub audio_backend: *const c_char,
    pub enable_plugins: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct mdaw_note {
    pub id: u64,
    pub start_tick: i64,
    pub length_ticks: i64,
    pub pitch: u8,
    pub velocity: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct mdaw_engine_state {
    pub playing: i32,
    pub position_tick: i64,
    pub position_seconds: f64,
    pub device_open: i32,
    pub sample_rate: f64,
    pub block_size: i32,
    pub xruns: i32,
    pub cpu: f32,
    pub peak_left: f32,
    pub peak_right: f32,
}

/// `mdaw_event_kind`, as the `kind` field of [`mdaw_event`] carries it.
pub type mdaw_event_kind = i32;
pub const MDAW_EVENT_NONE: mdaw_event_kind = 0;
pub const MDAW_EVENT_PARAM_CHANGED: mdaw_event_kind = 1;
pub const MDAW_EVENT_STATE_CHANGED: mdaw_event_kind = 2;
pub const MDAW_EVENT_EDITOR_CLOSED: mdaw_event_kind = 3;
pub const MDAW_EVENT_DEVICE_CHANGED: mdaw_event_kind = 4;
pub const MDAW_EVENT_AUDIO_STALLED: mdaw_event_kind = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct mdaw_event {
    pub kind: i32,
    pub device: u64,
    pub value: f32,
    pub param_id: [c_char; 64],
}

impl Default for mdaw_event {
    fn default() -> Self {
        Self {
            kind: MDAW_EVENT_NONE,
            device: 0,
            value: 0.0,
            param_id: [0; 64],
        }
    }
}

unsafe extern "C" {
    // ---- life cycle
    pub fn mdaw_engine_create(config: *const mdaw_engine_config) -> *mut mdaw_engine;
    pub fn mdaw_engine_destroy(engine: *mut mdaw_engine);
    pub safe fn mdaw_abi_version() -> u32;
    pub fn mdaw_last_error(buffer: *mut c_char, capacity: usize) -> usize;
    pub fn mdaw_engine_versions(buffer: *mut c_char, capacity: usize) -> usize;

    // ---- the project
    pub fn mdaw_project_clear(engine: *mut mdaw_engine) -> mdaw_status;
    pub fn mdaw_tempo_put(engine: *mut mdaw_engine, tempo_id: u64, tick: i64, milli_bpm: u32) -> mdaw_status;
    pub fn mdaw_tempo_remove(engine: *mut mdaw_engine, tempo_id: u64) -> mdaw_status;
    pub fn mdaw_master_set_gain(engine: *mut mdaw_engine, gain: f32) -> mdaw_status;

    pub fn mdaw_track_add(
        engine: *mut mdaw_engine,
        track: u64,
        after_track: u64,
        name: *const c_char,
    ) -> mdaw_status;
    pub fn mdaw_track_remove(engine: *mut mdaw_engine, track: u64) -> mdaw_status;
    pub fn mdaw_track_move(engine: *mut mdaw_engine, track: u64, after_track: u64) -> mdaw_status;
    pub fn mdaw_track_set_name(engine: *mut mdaw_engine, track: u64, name: *const c_char) -> mdaw_status;
    pub fn mdaw_track_set_mix(
        engine: *mut mdaw_engine,
        track: u64,
        gain: f32,
        pan: f32,
        mute: i32,
    ) -> mdaw_status;

    pub fn mdaw_clip_add(
        engine: *mut mdaw_engine,
        track: u64,
        clip: u64,
        start_tick: i64,
        length_ticks: i64,
        name: *const c_char,
    ) -> mdaw_status;
    pub fn mdaw_clip_remove(engine: *mut mdaw_engine, clip: u64) -> mdaw_status;
    pub fn mdaw_clip_set_range(
        engine: *mut mdaw_engine,
        clip: u64,
        start_tick: i64,
        length_ticks: i64,
    ) -> mdaw_status;

    pub fn mdaw_notes_put(
        engine: *mut mdaw_engine,
        clip: u64,
        notes: *const mdaw_note,
        count: usize,
    ) -> mdaw_status;
    pub fn mdaw_notes_remove(
        engine: *mut mdaw_engine,
        clip: u64,
        note_ids: *const u64,
        count: usize,
    ) -> mdaw_status;

    pub fn mdaw_device_add(
        engine: *mut mdaw_engine,
        track: u64,
        device: u64,
        after_device: u64,
        format: *const c_char,
        identifier: *const c_char,
    ) -> mdaw_status;
    pub fn mdaw_device_remove(engine: *mut mdaw_engine, device: u64) -> mdaw_status;
    pub fn mdaw_device_move(engine: *mut mdaw_engine, device: u64, after_device: u64) -> mdaw_status;
    pub fn mdaw_device_set_bypass(engine: *mut mdaw_engine, device: u64, bypass: i32) -> mdaw_status;
    pub fn mdaw_device_set_param(
        engine: *mut mdaw_engine,
        device: u64,
        param_id: *const c_char,
        value: f32,
    ) -> mdaw_status;
    pub fn mdaw_device_get_param(
        engine: *mut mdaw_engine,
        device: u64,
        param_id: *const c_char,
        value: *mut f32,
    ) -> mdaw_status;
    pub fn mdaw_device_params(
        engine: *mut mdaw_engine,
        device: u64,
        buffer: *mut c_char,
        capacity: usize,
    ) -> usize;
    pub fn mdaw_device_get_state(
        engine: *mut mdaw_engine,
        device: u64,
        buffer: *mut u8,
        capacity: usize,
    ) -> usize;
    pub fn mdaw_device_set_state(
        engine: *mut mdaw_engine,
        device: u64,
        bytes: *const u8,
        length: usize,
    ) -> mdaw_status;
    pub fn mdaw_device_show_editor(engine: *mut mdaw_engine, device: u64, show: i32) -> mdaw_status;

    // ---- plugins on this machine
    pub fn mdaw_plugins_scan(engine: *mut mdaw_engine, format: *const c_char) -> mdaw_status;
    pub fn mdaw_plugins_list(engine: *mut mdaw_engine, buffer: *mut c_char, capacity: usize) -> usize;

    // ---- local playback
    pub fn mdaw_transport_play(engine: *mut mdaw_engine) -> mdaw_status;
    pub fn mdaw_transport_stop(engine: *mut mdaw_engine) -> mdaw_status;
    pub fn mdaw_transport_seek(engine: *mut mdaw_engine, tick: i64) -> mdaw_status;
    pub fn mdaw_transport_set_loop(engine: *mut mdaw_engine, start_tick: i64, end_tick: i64) -> mdaw_status;
    pub fn mdaw_engine_state_read(engine: *mut mdaw_engine, out: *mut mdaw_engine_state);
    pub fn mdaw_audio_devices(engine: *mut mdaw_engine, buffer: *mut c_char, capacity: usize) -> usize;
    pub fn mdaw_audio_device_set(
        engine: *mut mdaw_engine,
        backend: *const c_char,
        output_device: *const c_char,
        sample_rate: f64,
        block_size: i32,
    ) -> mdaw_status;
    pub fn mdaw_render(
        engine: *mut mdaw_engine,
        wav_path: *const c_char,
        start_tick: i64,
        end_tick: i64,
        sample_rate: f64,
    ) -> mdaw_status;

    // ---- events
    pub fn mdaw_engine_poll_event(engine: *mut mdaw_engine, out: *mut mdaw_event) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn abi_version_matches() {
        assert_eq!(mdaw_abi_version(), MDAW_ABI_VERSION);
    }

    /// Verified against the header with a C program (`sizeof`, `_Alignof`, `offsetof`) on
    /// x86-64 Linux with GCC.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn layouts_match_the_c_header() {
        assert_eq!(
            (size_of::<mdaw_engine_config>(), align_of::<mdaw_engine_config>()),
            (48, 8)
        );
        assert_eq!(
            [
                offset_of!(mdaw_engine_config, app_name),
                offset_of!(mdaw_engine_config, settings_dir),
                offset_of!(mdaw_engine_config, open_audio_device),
                offset_of!(mdaw_engine_config, audio_backend),
                offset_of!(mdaw_engine_config, enable_plugins),
            ],
            [8, 16, 24, 32, 40]
        );

        assert_eq!((size_of::<mdaw_note>(), align_of::<mdaw_note>()), (32, 8));
        assert_eq!(
            [
                offset_of!(mdaw_note, start_tick),
                offset_of!(mdaw_note, length_ticks),
                offset_of!(mdaw_note, pitch),
                offset_of!(mdaw_note, velocity),
            ],
            [8, 16, 24, 25]
        );

        assert_eq!(
            (size_of::<mdaw_engine_state>(), align_of::<mdaw_engine_state>()),
            (64, 8)
        );
        assert_eq!(
            [
                offset_of!(mdaw_engine_state, position_tick),
                offset_of!(mdaw_engine_state, position_seconds),
                offset_of!(mdaw_engine_state, device_open),
                offset_of!(mdaw_engine_state, sample_rate),
                offset_of!(mdaw_engine_state, block_size),
                offset_of!(mdaw_engine_state, xruns),
                offset_of!(mdaw_engine_state, cpu),
                offset_of!(mdaw_engine_state, peak_left),
                offset_of!(mdaw_engine_state, peak_right),
            ],
            [8, 16, 24, 32, 40, 44, 48, 52, 56]
        );

        assert_eq!((size_of::<mdaw_event>(), align_of::<mdaw_event>()), (88, 8));
        assert_eq!(
            [
                offset_of!(mdaw_event, device),
                offset_of!(mdaw_event, value),
                offset_of!(mdaw_event, param_id),
            ],
            [8, 16, 20]
        );

        // The C enums are int-sized.
        assert_eq!(size_of::<mdaw_status>(), 4);
    }
}
