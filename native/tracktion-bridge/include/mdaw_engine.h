/* mdaw_engine.h — the whole interface between the Rust application and the local audio engine
 * (Tracktion Engine). C, plain data, opaque handle. See docs/tracktion-integration.md.
 *
 * Rules
 *  - Threads. Every function may be called from any thread except a real-time audio thread.
 *    The engine runs its own message thread; a call is carried out there and returns when it is
 *    done. Calls are therefore serialised and may take milliseconds (loading a plugin: longer).
 *  - No callbacks. The engine never calls into the application, from any thread. What the
 *    engine has to say is read with mdaw_engine_state() and mdaw_engine_poll_event().
 *  - No exceptions cross this interface. Functions return MDAW_OK or an error code;
 *    mdaw_last_error() gives the text for the calling thread's last failed call.
 *  - Strings are UTF-8, NUL-terminated, owned by the caller and only read during the call.
 *    Text and bytes coming back are copied into a buffer the caller supplies: the function
 *    returns the full length needed (excluding the NUL for text); if that exceeds the capacity
 *    given, nothing useful was written and the caller calls again with a larger buffer.
 *  - Identity. Tracks, clips, notes, devices and tempo points are named by the application's
 *    own 64-bit ids. The engine keeps the mapping to its objects. Nothing is addressed by
 *    position. Id 0 means "none"; adding something with id 0 is MDAW_ERR_ARGUMENT.
 *  - Time in the project is ticks at 960 per quarter note. Transport position is local to this
 *    engine and never part of the project.
 *  - Applying the same operation twice, or one whose target is gone, does nothing and returns
 *    MDAW_OK (as the application's own operations do), except where noted. Values out of
 *    range (pitch over 127, velocity 0, length 0, tempo outside 20..300 bpm, negative gain,
 *    pan outside -1..1) are MDAW_ERR_ARGUMENT and change nothing.
 */
#ifndef MDAW_ENGINE_H
#define MDAW_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MDAW_ABI_VERSION 2
#define MDAW_TICKS_PER_QUARTER 960

typedef struct mdaw_engine mdaw_engine;

typedef enum mdaw_status {
    MDAW_OK = 0,
    MDAW_ERR_ARGUMENT = 1,   /* null pointer, bad value */
    MDAW_ERR_NOT_FOUND = 2,  /* a plugin, a device or a file that had to exist does not */
    MDAW_ERR_DEVICE = 3,     /* the audio device could not be opened or changed */
    MDAW_ERR_PLUGIN = 4,     /* the plugin could not be loaded */
    MDAW_ERR_IO = 5,         /* a file could not be read or written */
    MDAW_ERR_INTERNAL = 6    /* anything else, including a C++ exception caught at the boundary */
} mdaw_status;

/* ---- life cycle ------------------------------------------------------------------------- */

typedef struct mdaw_engine_config {
    uint32_t abi_version;      /* MDAW_ABI_VERSION */
    const char* app_name;      /* window titles, the engine's application name */
    const char* settings_dir;  /* where the engine keeps its own settings; created if missing */
    int32_t open_audio_device; /* 0: no device (tests, offline render); 1: open the default.
                                  If it cannot be opened the engine is still created; see
                                  mdaw_engine_state.device_open */
    const char* audio_backend; /* "ALSA", "JACK", ... or NULL for the platform default */
    int32_t enable_plugins;    /* 0: built-in instruments only; 1: VST3, LV2 and CLAP as well */
} mdaw_engine_config;

/* One engine per process. Returns NULL on failure (see mdaw_last_error). */
mdaw_engine* mdaw_engine_create(const mdaw_engine_config* config);
/* Stops playback, closes editors and the device, and joins the engine's threads. */
void mdaw_engine_destroy(mdaw_engine* engine);

uint32_t mdaw_abi_version(void);
/* Text of the calling thread's last failed call; returns the length needed. */
size_t mdaw_last_error(char* buffer, size_t capacity);
/* Status of the calling thread's last failed call, for functions that cannot return one
 * (mdaw_engine_create, the functions returning a length). MDAW_OK if none failed yet; a
 * later successful call does not clear it. */
mdaw_status mdaw_last_status(void);
/* "Tracktion Engine v… / JUCE v…"; returns the length needed. */
size_t mdaw_engine_versions(char* buffer, size_t capacity);

/* ---- the project: derived from the application's canonical project ----------------------- */

typedef enum mdaw_entity_kind {
    MDAW_ENTITY_TRACK = 1,
    MDAW_ENTITY_CLIP = 2,
    MDAW_ENTITY_DEVICE = 3,
    MDAW_ENTITY_TEMPO = 4
} mdaw_entity_kind;
/* 1 if the engine has something of this kind under this id, 0 if not (or on bad arguments). */
int32_t mdaw_project_has(mdaw_engine* engine, int32_t kind, uint64_t id);

/* Removes everything: tracks, tempo points, mappings. Playback stops. */
mdaw_status mdaw_project_clear(mdaw_engine* engine);

/* Tempo map. A point at tick 0 always exists (120 bpm until put). bpm in thousandths. One
 * point per tick: a put at a tick another id holds replaces that point. Tempo changes in steps.
 * The engine plays 20 to 300 bpm; anything else is MDAW_ERR_ARGUMENT. */
mdaw_status mdaw_tempo_put(mdaw_engine* engine, uint64_t tempo_id, int64_t tick, uint32_t milli_bpm);
mdaw_status mdaw_tempo_remove(mdaw_engine* engine, uint64_t tempo_id);

mdaw_status mdaw_master_set_gain(mdaw_engine* engine, float gain /* linear, 1 = unity */);

/* after_track 0 puts the track first. */
mdaw_status mdaw_track_add(mdaw_engine* engine, uint64_t track, uint64_t after_track, const char* name);
mdaw_status mdaw_track_remove(mdaw_engine* engine, uint64_t track);
mdaw_status mdaw_track_move(mdaw_engine* engine, uint64_t track, uint64_t after_track);
mdaw_status mdaw_track_set_name(mdaw_engine* engine, uint64_t track, const char* name);
mdaw_status mdaw_track_set_mix(mdaw_engine* engine, uint64_t track,
                               float gain /* linear */, float pan /* -1..1 */, int32_t mute);

/* MIDI clips. */
mdaw_status mdaw_clip_add(mdaw_engine* engine, uint64_t track, uint64_t clip,
                          int64_t start_tick, int64_t length_ticks, const char* name);
mdaw_status mdaw_clip_remove(mdaw_engine* engine, uint64_t clip);
mdaw_status mdaw_clip_set_range(mdaw_engine* engine, uint64_t clip, int64_t start_tick, int64_t length_ticks);
mdaw_status mdaw_clip_set_name(mdaw_engine* engine, uint64_t clip, const char* name);

/* Notes; start is relative to the clip's start. velocity 1..127. */
typedef struct mdaw_note {
    uint64_t id;
    int64_t start_tick;
    int64_t length_ticks;
    uint8_t pitch;
    uint8_t velocity;
} mdaw_note;
/* Adds the notes that are new and updates those whose id the clip already has. */
mdaw_status mdaw_notes_put(mdaw_engine* engine, uint64_t clip, const mdaw_note* notes, size_t count);
mdaw_status mdaw_notes_remove(mdaw_engine* engine, uint64_t clip, const uint64_t* note_ids, size_t count);

/* Devices: instruments and effects on a track, in order.
 * format "builtin" with identifier "4osc" is the engine's own synth and always available.
 * Otherwise format is "VST3", "LV2" or "CLAP" and identifier is the plugin's identifier as
 * mdaw_plugins_list reports it: the plugin's OWN id, the same on every machine that has the
 * plugin installed, wherever it is installed — for CLAP the plugin id ("org.example.synth"),
 * for LV2 the plugin URI, for VST3 the class id as 32 hex digits. The pair (format, identifier)
 * is what a shared project stores. A plugin that is not installed gives MDAW_ERR_NOT_FOUND, one
 * that fails to load MDAW_ERR_PLUGIN; in both cases the device is NOT created. */
mdaw_status mdaw_device_add(mdaw_engine* engine, uint64_t track, uint64_t device, uint64_t after_device,
                            const char* format, const char* identifier);
mdaw_status mdaw_device_remove(mdaw_engine* engine, uint64_t device);
mdaw_status mdaw_device_move(mdaw_engine* engine, uint64_t device, uint64_t after_device);
mdaw_status mdaw_device_set_bypass(mdaw_engine* engine, uint64_t device, int32_t bypass);
/* Parameters are named by the plugin's own parameter id, never by name or index (for CLAP the
 * decimal clap_id, for LV2 "sym:" and the port symbol). value is normalised 0..1. Unknown parameter: MDAW_ERR_NOT_FOUND; so is
 * mdaw_device_get_param on a device that does not exist, as it has no value to give.
 * A value set here reaches the plugin with the next block it processes: state read before
 * that still holds the old value. */
mdaw_status mdaw_device_set_param(mdaw_engine* engine, uint64_t device, const char* param_id, float value);
mdaw_status mdaw_device_get_param(mdaw_engine* engine, uint64_t device, const char* param_id, float* value);
/* JSON array of {"id","name","value","default","steps","min","max","text"}; returns the
 * length needed. value and default are normalised. min and max are the plugin's own range for
 * the parameter where the format has one (CLAP), so that plain = min + normalised * (max - min);
 * otherwise 0 and 1. steps is 0 for a continuous parameter. text is the current value as the
 * plugin displays it ("440 Hz"). */
size_t mdaw_device_params(mdaw_engine* engine, uint64_t device, char* buffer, size_t capacity);
/* How the plugin displays this normalised value of the parameter; returns the length needed. */
size_t mdaw_device_param_text(mdaw_engine* engine, uint64_t device, const char* param_id, float value,
                              char* buffer, size_t capacity);
/* The plugin's opaque state. Reading flushes it from the plugin first. Returns the length
 * needed; 0 if the device does not exist or has no state. */
size_t mdaw_device_get_state(mdaw_engine* engine, uint64_t device, uint8_t* buffer, size_t capacity);
mdaw_status mdaw_device_set_state(mdaw_engine* engine, uint64_t device, const uint8_t* bytes, size_t length);
/* Back to the state the plugin had when the device was made. The device keeps its id and
 * place. A plugin that saves no state at all cannot be reset: MDAW_ERR_PLUGIN. */
mdaw_status mdaw_device_reset_state(mdaw_engine* engine, uint64_t device);
/* The plugin's own window. Opening one that is open brings it to the front. */
mdaw_status mdaw_device_show_editor(mdaw_engine* engine, uint64_t device, int32_t show);

/* ---- plugins on this machine -------------------------------------------------------------- */

/* Scans the usual folders for one format ("VST3", "LV2", "CLAP"). Blocks until done. For now
 * in this process: a plugin that crashes while being scanned takes the application with it. */
mdaw_status mdaw_plugins_scan(mdaw_engine* engine, const char* format);
/* JSON array of {"format","identifier","name","vendor","instrument"}; returns the length needed.
 * identifier is as described at mdaw_device_add. The engine remembers scan results between
 * runs (in settings_dir), but lists only plugins it can load now: the format is enabled and
 * the plugin's file is still there. */
size_t mdaw_plugins_list(mdaw_engine* engine, char* buffer, size_t capacity);

/* ---- local playback: never part of the project ------------------------------------------- */

mdaw_status mdaw_transport_play(mdaw_engine* engine);
mdaw_status mdaw_transport_stop(mdaw_engine* engine);
mdaw_status mdaw_transport_seek(mdaw_engine* engine, int64_t tick);
/* end_tick <= start_tick switches looping off.
 * Without an open audio device nothing plays: play is remembered, the position stays where it
 * was last put, and playback starts from there once a device is open. */
mdaw_status mdaw_transport_set_loop(mdaw_engine* engine, int64_t start_tick, int64_t end_tick);

typedef struct mdaw_engine_state {
    int32_t playing;          /* what the application asked for; the engine keeps it so */
    int64_t position_tick;
    double position_seconds;
    int32_t device_open;
    double sample_rate;
    int32_t block_size;
    int32_t xruns;            /* since the device was opened */
    float cpu;                /* 0..1 */
    float peak_left, peak_right; /* master output since the last call, linear */
} mdaw_engine_state;
/* Cheap; meant to be called every UI frame. Reads atomics, does not wait for the message thread. */
void mdaw_engine_state_read(mdaw_engine* engine, mdaw_engine_state* out);

/* JSON: {"backends":[{"name","outputs":[..],"inputs":[..]}],"midi_inputs":[..],"current":{..}}. */
size_t mdaw_audio_devices(mdaw_engine* engine, char* buffer, size_t capacity);
/* NULL or 0 leaves a setting as it is. */
mdaw_status mdaw_audio_device_set(mdaw_engine* engine, const char* backend, const char* output_device,
                                  double sample_rate, int32_t block_size);

/* Renders ticks [start, end) of the project to a 24-bit stereo wav file, faster than real time,
 * without the audio device. Playback is stopped for the duration. */
mdaw_status mdaw_render(mdaw_engine* engine, const char* wav_path, int64_t start_tick, int64_t end_tick,
                        double sample_rate);

/* ---- what the engine has to tell ----------------------------------------------------------- */

typedef enum mdaw_event_kind {
    MDAW_EVENT_NONE = 0,
    MDAW_EVENT_PARAM_CHANGED = 1,   /* in the plugin's own window; device, param_id, value */
    MDAW_EVENT_STATE_CHANGED = 2,   /* the plugin's state changed otherwise (preset loaded); device */
    MDAW_EVENT_EDITOR_CLOSED = 3,   /* the user closed the plugin's window; device */
    MDAW_EVENT_DEVICE_CHANGED = 4,  /* the audio device or the device list changed */
    MDAW_EVENT_AUDIO_STALLED = 5    /* the audio callback has not returned for over a second */
} mdaw_event_kind;

typedef struct mdaw_event {
    int32_t kind;        /* mdaw_event_kind */
    uint64_t device;
    float value;
    char param_id[64];
} mdaw_event;
/* Returns 1 and fills *out if there was an event, 0 if not. Events caused by the application's
 * own calls (mdaw_device_set_param ...) are not reported. Call until it returns 0, every frame. */
int32_t mdaw_engine_poll_event(mdaw_engine* engine, mdaw_event* out);

#ifdef __cplusplus
}
#endif
#endif
