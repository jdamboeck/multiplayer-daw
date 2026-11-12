/* Exercises the Tracktion bridge through mdaw_engine.h alone, written in C to show that the
 * header is usable from C. No audio device is opened. Prints `RESULT key=value` lines and exits
 * non-zero if a check fails. Run from the repository root; files go to target/bridge-scratch/.
 * The CLAP test synth is used if CLAP_PATH points at a folder with it (see the bridge brief). */
#define _POSIX_C_SOURCE 200809L
#include "mdaw_engine.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define SCRATCH "target/bridge-scratch"
#define Q MDAW_TICKS_PER_QUARTER

static int failures = 0;

static void check (int ok, const char* name)
{
    printf ("RESULT %s=%d\n", name, ok ? 1 : 0);
    if (! ok) ++failures;
}

static void measure (const char* name, double value) { printf ("RESULT %s=%.6g\n", name, value); }

static void sleepMs (int ms)
{
    struct timespec t = { ms / 1000, (long) (ms % 1000) * 1000000L };
    nanosleep (&t, NULL);
}

static void printLastError (const char* context)
{
    char text[1024];
    mdaw_last_error (text, sizeof text);
    printf ("error after %s: %s\n", context, text);
}

/* -------- wav files ---------------------------------------------------------------------- */

typedef struct
{
    int ok, channels, rate, bits;
    long frames;
    float* samples; /* interleaved */
} Wav;

static unsigned le16 (const unsigned char* p) { return (unsigned) p[0] | ((unsigned) p[1] << 8); }
static unsigned long le32 (const unsigned char* p) { return (unsigned long) p[0] | ((unsigned long) p[1] << 8) | ((unsigned long) p[2] << 16) | ((unsigned long) p[3] << 24); }

/* Reads a 24-bit PCM wav file. */
static Wav readWav (const char* path)
{
    Wav w = { 0 };
    FILE* f = fopen (path, "rb");
    if (f == NULL) return w;
    fseek (f, 0, SEEK_END);
    long size = ftell (f);
    fseek (f, 0, SEEK_SET);
    unsigned char* data = malloc ((size_t) size);
    if (data == NULL || fread (data, 1, (size_t) size, f) != (size_t) size) { fclose (f); free (data); return w; }
    fclose (f);

    if (size < 12 || memcmp (data, "RIFF", 4) != 0 || memcmp (data + 8, "WAVE", 4) != 0) { free (data); return w; }
    long pos = 12;
    const unsigned char* pcm = NULL;
    unsigned long pcmBytes = 0;
    while (pos + 8 <= size)
    {
        unsigned long chunk = le32 (data + pos + 4);
        const unsigned char* body = data + pos + 8;
        if ((long) chunk > size - pos - 8) chunk = (unsigned long) (size - pos - 8);
        if (memcmp (data + pos, "fmt ", 4) == 0 && chunk >= 16)
        {
            unsigned format = le16 (body);
            w.channels = (int) le16 (body + 2);
            w.rate = (int) le32 (body + 4);
            w.bits = (int) le16 (body + 14);
            if (format != 1 && format != 0xFFFE) w.bits = -1;
        }
        else if (memcmp (data + pos, "data", 4) == 0)
        {
            pcm = body;
            pcmBytes = chunk;
        }
        pos += 8 + (long) chunk + (long) (chunk & 1);
    }
    if (pcm != NULL && w.bits == 24 && w.channels > 0)
    {
        w.frames = (long) (pcmBytes / (3UL * (unsigned long) w.channels));
        w.samples = malloc (sizeof (float) * (size_t) (w.frames * w.channels + 1));
        for (long i = 0; i < w.frames * w.channels; ++i)
        {
            const unsigned char* s = pcm + 3 * i;
            long v = (long) s[0] | ((long) s[1] << 8) | ((long) s[2] << 16);
            if (v & 0x800000L) v -= 0x1000000L;
            w.samples[i] = (float) v / 8388608.0f;
        }
        w.ok = 1;
    }
    free (data);
    return w;
}

static double peakBetween (const Wav* w, double fromSeconds, double toSeconds)
{
    if (! w->ok) return -1.0;
    long a = (long) (fromSeconds * w->rate), b = (long) (toSeconds * w->rate);
    if (a < 0) a = 0;
    if (b > w->frames) b = w->frames;
    double peak = 0.0;
    for (long i = a * w->channels; i < b * w->channels; ++i)
        if (fabs (w->samples[i]) > peak) peak = fabs (w->samples[i]);
    return peak;
}

static double peakOf (const Wav* w) { return peakBetween (w, 0.0, 1.0e9); }

/* Seconds of the first and last sample above the threshold; -1 if none. */
static double firstAbove (const Wav* w, double threshold)
{
    for (long i = 0; w->ok && i < w->frames * w->channels; ++i)
        if (fabs (w->samples[i]) > threshold) return (double) (i / w->channels) / w->rate;
    return -1.0;
}

static double lastAbove (const Wav* w, double threshold)
{
    for (long i = w->ok ? w->frames * w->channels - 1 : -1; i >= 0; --i)
        if (fabs (w->samples[i]) > threshold) return (double) (i / w->channels) / w->rate;
    return -1.0;
}

static double seconds (const Wav* w) { return w->ok ? (double) w->frames / w->rate : -1.0; }

static Wav render (mdaw_engine* e, const char* name, int64_t start, int64_t end)
{
    char path[512];
    snprintf (path, sizeof path, SCRATCH "/%s.wav", name);
    remove (path);
    mdaw_status s = mdaw_render (e, path, start, end, 44100.0);
    if (s != MDAW_OK) { printLastError (name); Wav none = { 0 }; return none; }
    return readWav (path);
}

static void freeWav (Wav* w) { free (w->samples); w->samples = NULL; w->ok = 0; }

/* -------- text results ------------------------------------------------------------------- */

static char* paramsJson (mdaw_engine* e, uint64_t device)
{
    size_t n = mdaw_device_params (e, device, NULL, 0);
    char* text = malloc (n + 1);
    text[0] = 0;
    mdaw_device_params (e, device, text, n + 1);
    return text;
}

static char* pluginsJson (mdaw_engine* e)
{
    size_t n = mdaw_plugins_list (e, NULL, 0);
    char* text = malloc (n + 1);
    text[0] = 0;
    mdaw_plugins_list (e, text, n + 1);
    return text;
}

/* The string value of "key" at or after `from`, copied into out. Returns where it ended. */
static const char* stringValue (const char* from, const char* key, char* out, size_t capacity)
{
    char quoted[64];
    snprintf (quoted, sizeof quoted, "\"%s\"", key);
    const char* p = strstr (from, quoted);
    if (p == NULL) return NULL;
    p += strlen (quoted);
    while (*p == ' ' || *p == ':') ++p;
    if (*p != '"') return NULL;
    ++p;
    size_t n = 0;
    while (*p != 0 && *p != '"' && n + 1 < capacity) out[n++] = *p++;
    out[n] = 0;
    return p;
}

static long numberValue (const char* from, const char* key)
{
    char quoted[64];
    snprintf (quoted, sizeof quoted, "\"%s\"", key);
    const char* p = strstr (from, quoted);
    if (p == NULL) return -1;
    p += strlen (quoted);
    while (*p == ' ' || *p == ':') ++p;
    return strtol (p, NULL, 10);
}

/* The id of the first parameter that is continuous ("steps": 0). */
static int continuousParam (const char* json, char* id, size_t capacity)
{
    const char* p = json;
    while ((p = strchr (p, '{')) != NULL)
    {
        const char* end = strchr (p, '}');
        if (end == NULL) return 0;
        if (stringValue (p, "id", id, capacity) != NULL && numberValue (p, "steps") == 0 && strstr (p, "\"steps\"") < end)
            return 1;
        p = end;
    }
    return 0;
}

/* Reads all queued events; returns how many were parameter or state changes. */
static int drainEvents (mdaw_engine* e)
{
    mdaw_event ev;
    int n = 0;
    while (mdaw_engine_poll_event (e, &ev))
    {
        printf ("event kind=%d device=%llu param=%s value=%g\n", ev.kind, (unsigned long long) ev.device, ev.param_id, ev.value);
        if (ev.kind == MDAW_EVENT_PARAM_CHANGED || ev.kind == MDAW_EVENT_STATE_CHANGED) ++n;
    }
    return n;
}

/* -------- the project used throughout ---------------------------------------------------- */

enum { TRACK = 10, SYNTH = 100, CLIP = 20, TEMPO0 = 1, TEMPO2 = 2, EXT_TRACK = 11, EXT = 101, EXT_CLIP = 21 };

static void quarterNotes (mdaw_note* notes, uint64_t firstId)
{
    for (int i = 0; i < 8; ++i)
    {
        notes[i].id = firstId + (uint64_t) i;
        notes[i].start_tick = (int64_t) i * Q;
        notes[i].length_ticks = Q * 9 / 10;
        notes[i].pitch = (uint8_t) (60 + (i % 3) * 4);
        notes[i].velocity = 100;
    }
}

static void build (mdaw_engine* e, mdaw_note* notes)
{
    mdaw_status s = MDAW_OK;
    s |= mdaw_tempo_put (e, TEMPO0, 0, 120000);
    s |= mdaw_track_add (e, TRACK, 0, "Synth");
    s |= mdaw_device_add (e, TRACK, SYNTH, 0, "builtin", "4osc");
    s |= mdaw_clip_add (e, TRACK, CLIP, 0, 8 * Q, "Clip");
    s |= mdaw_notes_put (e, CLIP, notes, 8);
    if (s != MDAW_OK) printLastError ("build");
    check (s == MDAW_OK, "build_calls_ok");
}

static mdaw_engine* create (void)
{
    mdaw_engine_config config;
    memset (&config, 0, sizeof config);
    config.abi_version = MDAW_ABI_VERSION;
    config.app_name = "mdaw-bridge-test";
    config.settings_dir = SCRATCH "/settings";
    config.open_audio_device = 0;
    config.audio_backend = NULL;
    config.enable_plugins = 1;
    return mdaw_engine_create (&config);
}

int main (void)
{
    setvbuf (stdout, NULL, _IOLBF, 0);

    /* 1. Life cycle ------------------------------------------------------------------------ */
    mdaw_engine* e = create();
    check (e != NULL, "create");
    if (e == NULL) { printLastError ("create"); return 1; }
    {
        char versions[256];
        size_t n = mdaw_engine_versions (versions, sizeof versions);
        printf ("versions: %s\n", versions);
        check (n > 0 && n < sizeof versions, "versions");
        mdaw_engine* second = create();
        char error[512];
        size_t errorLength = mdaw_last_error (error, sizeof error);
        check (second == NULL && errorLength > 0, "second_create_refused");
        if (second != NULL) mdaw_engine_destroy (second);
    }

    /* 2. A project, rendered --------------------------------------------------------------- */
    mdaw_note notes[8];
    quarterNotes (notes, 1000);
    build (e, notes);
    Wav w = render (e, "first", 0, 8 * Q);
    measure ("first_seconds", seconds (&w));
    measure ("first_peak", peakOf (&w));
    check (w.ok && w.channels == 2 && w.bits == 24, "render_wav_24bit_stereo");
    check (fabs (seconds (&w) - 4.0) < 0.1, "render_length_4s");
    check (peakOf (&w) > 0.05, "render_peak");
    double firstPeak = peakOf (&w);
    freeWav (&w);

    /* 3. Idempotency: the same calls again change nothing --------------------------------- */
    build (e, notes);
    w = render (e, "again", 0, 8 * Q);
    measure ("again_peak", peakOf (&w));
    check (fabs (seconds (&w) - 4.0) < 0.1 && peakOf (&w) > 0.05 && peakOf (&w) < firstPeak * 2.0, "repeat_render_same");
    freeWav (&w);
    {
        uint64_t ids[8];
        for (int i = 0; i < 8; ++i) ids[i] = notes[i].id;
        check (mdaw_notes_remove (e, CLIP, ids, 8) == MDAW_OK, "notes_remove");
        /* Had the repeated put duplicated notes, removing each id once would leave some. */
        w = render (e, "removed", 0, 8 * Q);
        measure ("removed_peak", peakOf (&w));
        check (w.ok && peakOf (&w) < 0.001, "removed_notes_silent");
        freeWav (&w);
    }

    /* 4. Missing ids and bad arguments ------------------------------------------------------ */
    {
        mdaw_status s = MDAW_OK;
        mdaw_note n = { 5, 0, Q, 60, 100 };
        uint64_t one = 5;
        s |= mdaw_track_remove (e, 999);
        s |= mdaw_track_move (e, 999, 0);
        s |= mdaw_track_set_name (e, 999, "x");
        s |= mdaw_track_set_mix (e, 999, 1.0f, 0.0f, 0);
        s |= mdaw_clip_add (e, 999, 998, 0, Q, "x");
        s |= mdaw_clip_remove (e, 999);
        s |= mdaw_clip_set_range (e, 999, 0, Q);
        s |= mdaw_notes_put (e, 999, &n, 1);
        s |= mdaw_notes_remove (e, 999, &one, 1);
        s |= mdaw_notes_remove (e, CLIP, &one, 1);
        s |= mdaw_device_add (e, 999, 997, 0, "builtin", "4osc");
        s |= mdaw_device_remove (e, 999);
        s |= mdaw_device_move (e, 999, 0);
        s |= mdaw_device_set_bypass (e, 999, 1);
        s |= mdaw_device_set_param (e, 999, "x", 0.5f);
        s |= mdaw_device_show_editor (e, 999, 1);
        s |= mdaw_tempo_remove (e, 999);
        check (s == MDAW_OK, "missing_ids_ok");
        check (mdaw_device_get_state (e, 999, NULL, 0) == 0, "missing_device_no_state");

        char error[512];
        int bad = 1;
        error[0] = 0;
        bad &= mdaw_track_add (NULL, 1, 0, "x") == MDAW_ERR_ARGUMENT && mdaw_last_error (error, sizeof error) > 0;
        error[0] = 0;
        bad &= mdaw_notes_put (e, CLIP, NULL, 3) == MDAW_ERR_ARGUMENT && mdaw_last_error (error, sizeof error) > 0;
        bad &= mdaw_device_get_param (e, SYNTH, "x", NULL) == MDAW_ERR_ARGUMENT;
        bad &= mdaw_render (e, NULL, 0, Q, 44100.0) == MDAW_ERR_ARGUMENT;
        bad &= mdaw_device_add (e, TRACK, 5, 0, NULL, "4osc") == MDAW_ERR_ARGUMENT;
        bad &= mdaw_tempo_put (e, 3, 0, 0) == MDAW_ERR_ARGUMENT;
        bad &= mdaw_clip_add (e, TRACK, 6, 0, 0, "empty") == MDAW_ERR_ARGUMENT;
        printf ("last error: %s\n", error);
        check (bad, "bad_arguments_refused");
        check (mdaw_device_add (e, TRACK, 7, 0, "builtin", "nonsense") == MDAW_ERR_NOT_FOUND, "unknown_builtin_not_found");
    }

    /* 5. Notes put again under their ids move instead of doubling ----------------------- */
    {
        for (int i = 0; i < 8; ++i)
        {
            notes[i].start_tick = (int64_t) i * Q / 2;   /* eighths in the first half */
            notes[i].length_ticks = Q * 2 / 5;
        }
        mdaw_notes_put (e, CLIP, notes, 8);
        w = render (e, "first_half", 0, 8 * Q);
        double early = peakBetween (&w, 0.0, 1.9), late = peakBetween (&w, 2.5, 4.0);
        measure ("first_half_early_peak", early);
        measure ("first_half_late_peak", late);
        check (early > 0.05 && late < 0.001, "notes_in_first_half");
        freeWav (&w);

        for (int i = 0; i < 8; ++i)
        {
            notes[i].start_tick = 4 * Q + (int64_t) i * Q / 2;   /* the same ids, second half */
            notes[i].pitch = (uint8_t) (notes[i].pitch + 7);
        }
        check (mdaw_notes_put (e, CLIP, notes, 8) == MDAW_OK, "notes_update");
        w = render (e, "second_half", 0, 8 * Q);
        early = peakBetween (&w, 0.0, 1.95);
        late = peakBetween (&w, 2.0, 4.0);
        measure ("second_half_early_peak", early);
        measure ("second_half_late_peak", late);
        check (early < 0.001 && late > 0.05, "notes_moved_not_doubled");
        freeWav (&w);
    }

    /* 6. Mute ------------------------------------------------------------------------------- */
    {
        mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 1);
        w = render (e, "muted", 0, 8 * Q);
        measure ("muted_peak", peakOf (&w));
        check (w.ok && peakOf (&w) < 0.001, "mute_silences");
        freeWav (&w);
        mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 0);
        w = render (e, "unmuted", 0, 8 * Q);
        measure ("unmuted_peak", peakOf (&w));
        check (peakOf (&w) > 0.05, "unmute_restores");
        freeWav (&w);
        mdaw_track_set_mix (e, TRACK, 0.25f, 0.0f, 0);
        w = render (e, "quieter", 0, 8 * Q);
        measure ("gain_quarter_peak", peakOf (&w));
        check (peakOf (&w) > 0.001 && peakOf (&w) < 0.6 * firstPeak, "gain_lowers_level");
        freeWav (&w);
        mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 0);
    }

    /* 6c. Bypass, master gain, moves, device listing ------------------------------------ */
    {
        mdaw_device_set_bypass (e, SYNTH, 1);
        w = render (e, "bypassed", 0, 8 * Q);
        measure ("bypassed_peak", peakOf (&w));
        check (w.ok && peakOf (&w) < 0.001, "bypass_silences");
        freeWav (&w);
        mdaw_device_set_bypass (e, SYNTH, 0);

        mdaw_master_set_gain (e, 0.25f);
        w = render (e, "master_quarter", 0, 8 * Q);
        measure ("master_quarter_peak", peakOf (&w));
        check (peakOf (&w) > 0.001 && peakOf (&w) < 0.6 * firstPeak, "master_gain_lowers_level");
        freeWav (&w);
        mdaw_master_set_gain (e, 1.0f);

        mdaw_status s = mdaw_device_add (e, TRACK, 103, SYNTH, "builtin", "4osc");
        s |= mdaw_device_move (e, 103, 0);
        s |= mdaw_device_move (e, SYNTH, 0);
        s |= mdaw_device_move (e, SYNTH, 103);
        s |= mdaw_device_remove (e, 103);
        s |= mdaw_track_add (e, 12, TRACK, "Empty");
        s |= mdaw_track_move (e, TRACK, 12);
        s |= mdaw_track_move (e, TRACK, 0);
        s |= mdaw_track_set_name (e, TRACK, "Renamed");
        s |= mdaw_track_remove (e, 12);
        check (s == MDAW_OK, "moves_ok");
        w = render (e, "after_moves", 0, 8 * Q);
        measure ("after_moves_peak", peakOf (&w));
        check (peakOf (&w) > 0.05, "after_moves_renders");
        freeWav (&w);

        size_t n = mdaw_audio_devices (e, NULL, 0);
        char* json = malloc (n + 1);
        json[0] = 0;
        mdaw_audio_devices (e, json, n + 1);
        printf ("audio devices: %s\n", json);
        check (n > 0 && json[0] == '{' && strstr (json, "\"backends\"") != NULL && strstr (json, "\"current\"") != NULL, "audio_devices_json");
        free (json);
    }

    /* 7. Tempo: notes keep their beats --------------------------------------------------- */
    {
        /* The notes now run from beat 4 to beat 7.9. */
        w = render (e, "tempo120", 0, 16 * Q);
        double start120 = firstAbove (&w, 0.001), end120 = lastAbove (&w, 0.001);
        measure ("tempo120_seconds", seconds (&w));
        measure ("tempo120_first_sound", start120);
        measure ("tempo120_last_sound", end120);
        freeWav (&w);

        mdaw_tempo_put (e, TEMPO0, 0, 60000);
        w = render (e, "tempo60", 0, 16 * Q);
        double start60 = firstAbove (&w, 0.001), end60 = lastAbove (&w, 0.001);
        measure ("tempo60_seconds", seconds (&w));
        measure ("tempo60_first_sound", start60);
        measure ("tempo60_last_sound", end60);
        check (fabs (seconds (&w) - 16.0) < 0.1, "tempo60_render_length");
        check (fabs (start120 - 2.0) < 0.05 && fabs (start60 - 4.0) < 0.05, "tempo_note_start_follows");
        /* Release tails are a fixed time, so the end moves by the notes' length in time (~3.9 s). */
        check (end60 - end120 > 3.5 && end60 - end120 < 4.3, "tempo_note_end_follows");
        freeWav (&w);

        /* A second point: 120 bpm up to beat 4, 60 from there. */
        mdaw_tempo_put (e, TEMPO0, 0, 120000);
        mdaw_tempo_put (e, TEMPO2, 4 * Q, 60000);
        w = render (e, "tempo_change", 0, 16 * Q);
        double startChange = firstAbove (&w, 0.001), endChange = lastAbove (&w, 0.001);
        measure ("tempo_change_seconds", seconds (&w));
        measure ("tempo_change_first_sound", startChange);
        measure ("tempo_change_last_sound", endChange);
        /* Beats 4 to 7.9 at 60 bpm, as in the 60 bpm render, but starting 2 s earlier. */
        check (fabs (seconds (&w) - 14.0) < 0.1 && fabs (startChange - 2.0) < 0.05 && fabs (endChange - (end60 - 2.0)) < 0.3, "tempo_point_inside");
        freeWav (&w);

        /* Moving the point (same id) to beat 8: everything plays at 120 again. */
        mdaw_tempo_put (e, TEMPO2, 8 * Q, 60000);
        w = render (e, "tempo_moved", 0, 8 * Q);
        measure ("tempo_moved_last_sound", lastAbove (&w, 0.001));
        check (fabs (lastAbove (&w, 0.001) - end120) < 0.3, "tempo_point_moved");
        freeWav (&w);
        mdaw_tempo_remove (e, TEMPO2);
        mdaw_tempo_remove (e, TEMPO0);   /* back to 120 */
        w = render (e, "tempo_removed", 0, 8 * Q);
        check (fabs (lastAbove (&w, 0.001) - end120) < 0.3, "tempo_removed_back_to_120");
        freeWav (&w);
    }

    /* 8. Transport without a device ----------------------------------------------------- */
    {
        mdaw_engine_state st;
        check (mdaw_transport_play (e) == MDAW_OK, "play_call");
        mdaw_engine_state_read (e, &st);
        check (st.playing == 1 && st.device_open == 0, "playing_without_device");
        check (mdaw_transport_seek (e, 4 * Q) == MDAW_OK, "seek_call");
        sleepMs (300);
        mdaw_engine_state_read (e, &st);
        measure ("position_tick_after_seek", (double) st.position_tick);
        measure ("position_seconds_after_seek", st.position_seconds);
        check (llabs (st.position_tick - 4 * Q) < Q / 4, "position_after_seek");
        sleepMs (500);
        mdaw_engine_state_read (e, &st);
        measure ("position_advanced_without_device", (double) (st.position_tick - 4 * Q));
        check (st.playing == 1, "still_wanted_playing");
        mdaw_transport_stop (e);
        mdaw_engine_state_read (e, &st);
        check (st.playing == 0, "stopped");
        check (mdaw_transport_set_loop (e, 0, 4 * Q) == MDAW_OK && mdaw_transport_set_loop (e, 0, 0) == MDAW_OK, "loop_calls");
    }

    /* 9. Parameters and state of the built-in synth -------------------------------------- */
    {
        char* json = paramsJson (e, SYNTH);
        char param[128] = "";
        int found = continuousParam (json, param, sizeof param);
        measure ("synth_params_json_bytes", (double) strlen (json));
        check (json[0] == '[' && strlen (json) > 10 && found, "synth_params_listed");
        printf ("param used: %s\n", param);
        free (json);

        float value = -1.0f;
        mdaw_status s = mdaw_device_set_param (e, SYNTH, param, 0.25f);
        s |= mdaw_device_get_param (e, SYNTH, param, &value);
        measure ("synth_param_read_back", value);
        check (s == MDAW_OK && fabsf (value - 0.25f) < 0.01f, "synth_param_set_get");
        check (mdaw_device_set_param (e, SYNTH, "no-such-param", 0.5f) == MDAW_ERR_NOT_FOUND, "unknown_param_not_found");

        size_t n = mdaw_device_get_state (e, SYNTH, NULL, 0);
        uint8_t* state = malloc (n + 1);
        size_t n2 = mdaw_device_get_state (e, SYNTH, state, n);
        measure ("synth_state_bytes", (double) n);
        check (n > 0 && n2 == n, "synth_state_read");
        mdaw_device_set_param (e, SYNTH, param, 0.75f);
        mdaw_device_get_param (e, SYNTH, param, &value);
        check (fabsf (value - 0.75f) < 0.01f, "synth_param_changed");
        s = mdaw_device_set_state (e, SYNTH, state, n);
        mdaw_device_get_param (e, SYNTH, param, &value);
        measure ("synth_param_after_restore", value);
        check (s == MDAW_OK && fabsf (value - 0.25f) < 0.01f, "synth_state_restores_param");
        free (state);

        /* 11. Our own changes are not reported back. */
        sleepMs (300);
        int events = drainEvents (e);
        measure ("events_after_own_changes", events);
        check (events == 0, "no_echo_events_builtin");
        check (mdaw_device_show_editor (e, SYNTH, 1) == MDAW_ERR_NOT_FOUND, "builtin_has_no_editor");
    }

    /* 10. An external plugin -------------------------------------------------------------- */
    {
        check (mdaw_device_add (e, TRACK, 102, 0, "CLAP", "nonsense") == MDAW_ERR_NOT_FOUND, "unknown_plugin_not_found");
        char* nothing = paramsJson (e, 102);
        check (strcmp (nothing, "[]") == 0 && mdaw_device_get_state (e, 102, NULL, 0) == 0, "unknown_plugin_created_nothing");
        free (nothing);

        mdaw_status scan = mdaw_plugins_scan (e, "CLAP");
        if (scan != MDAW_OK) printLastError ("scan");
        check (scan == MDAW_OK, "scan_clap");
        char* list = pluginsJson (e);
        char identifier[1024] = "";
        const char* name = strstr (list, "\"mdaw Test Synth\"");
        if (name != NULL)
        {
            const char* objectStart = name;
            while (objectStart > list && *objectStart != '{') --objectStart;
            stringValue (objectStart, "identifier", identifier, sizeof identifier);
        }
        free (list);

        if (identifier[0] == 0)
        {
            printf ("RESULT external_skipped=1\n");
        }
        else
        {
            printf ("external identifier: %s\n", identifier);
            mdaw_note extNotes[8];
            quarterNotes (extNotes, 2000);
            mdaw_status s = mdaw_track_add (e, EXT_TRACK, TRACK, "External");
            s |= mdaw_device_add (e, EXT_TRACK, EXT, 0, "CLAP", identifier);
            if (s != MDAW_OK) printLastError ("external device_add");
            s |= mdaw_clip_add (e, EXT_TRACK, EXT_CLIP, 0, 8 * Q, "Ext");
            s |= mdaw_notes_put (e, EXT_CLIP, extNotes, 8);
            check (s == MDAW_OK, "external_added");

            mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 1);   /* only the external synth is heard */
            w = render (e, "external", 0, 8 * Q);
            measure ("external_peak", peakOf (&w));
            check (peakOf (&w) > 0.01, "external_renders");
            freeWav (&w);
            mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 0);

            char* json = paramsJson (e, EXT);
            char param[128] = "";
            int found = continuousParam (json, param, sizeof param);
            printf ("external params: %s\n", json);
            check (found, "external_params_listed");
            free (json);
            if (found)
            {
                float value = -1.0f;
                mdaw_device_set_param (e, EXT, param, 0.3f);
                mdaw_device_get_param (e, EXT, param, &value);
                measure ("external_param_read_back", value);
                check (fabsf (value - 0.3f) < 0.01f, "external_param_set_get");

                size_t n = mdaw_device_get_state (e, EXT, NULL, 0);
                uint8_t* state = malloc (n + 1);
                mdaw_device_get_state (e, EXT, state, n);
                measure ("external_state_bytes", (double) n);
                mdaw_device_set_param (e, EXT, param, 0.8f);
                mdaw_status r = mdaw_device_set_state (e, EXT, state, n);
                mdaw_device_get_param (e, EXT, param, &value);
                measure ("external_param_after_restore", value);
                /* The value set just before the state was read may not have reached the plugin
                 * yet (no block processed in between), so this is reported, not required. */
                printf ("RESULT external_state_restore_ok=%d\n", r == MDAW_OK);
                free (state);

                sleepMs (400);
                int events = drainEvents (e);
                measure ("events_after_own_external_changes", events);
                check (events == 0, "no_echo_events_external");
            }
            /* Reported only: whether the test synth has a window depends on the plugin. */
            mdaw_status editor = mdaw_device_show_editor (e, EXT, 1);
            printf ("RESULT external_editor_status=%d\n", (int) editor);
            if (editor == MDAW_OK) mdaw_device_show_editor (e, EXT, 0);
            check (mdaw_device_remove (e, EXT) == MDAW_OK, "external_removed");
        }
    }

    /* 6b. Removing the tracks leaves silence -------------------------------------------- */
    {
        mdaw_track_remove (e, EXT_TRACK);
        mdaw_track_remove (e, TRACK);
        w = render (e, "no_tracks", 0, 8 * Q);
        measure ("no_tracks_peak", peakOf (&w));
        check (w.ok && peakOf (&w) < 0.001, "track_remove_silences");
        freeWav (&w);
        check (mdaw_project_clear (e) == MDAW_OK, "project_clear");
    }

    /* 12. Destroy, and a second life ---------------------------------------------------- */
    mdaw_engine_destroy (e);
    check (1, "destroyed");
    e = create();
    if (e == NULL) printLastError ("second create");
    check (e != NULL, "second_life_create");
    if (e != NULL)
    {
        quarterNotes (notes, 1000);
        build (e, notes);
        w = render (e, "second_life", 0, 8 * Q);
        measure ("second_life_peak", peakOf (&w));
        check (peakOf (&w) > 0.05, "second_life_renders");
        freeWav (&w);
        mdaw_engine_destroy (e);
        check (1, "second_life_destroyed");
    }

    printf ("RESULT failures=%d\n", failures);
    printf ("RESULT ok=%d\n", failures == 0 ? 1 : 0);
    return failures == 0 ? 0 : 1;
}
