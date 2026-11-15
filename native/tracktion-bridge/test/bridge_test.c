/* Exercises the Tracktion bridge through mdaw_engine.h alone, written in C to show that the
 * header is usable from C. No audio device is opened. Prints `RESULT key=value` lines and exits
 * non-zero if a check fails. Run from the repository root; files go to target/bridge-scratch/.
 * The CLAP test synth is used if CLAP_PATH points at a folder with it (see the bridge brief).
 * The plugin scanner and the test plugins of test/bad_clap.c are found where CMake built them
 * (MDAW_TEST_SCANNER, MDAW_TEST_CLAP_DIR, compiled in; the environment variables of the same
 * names override them).
 *
 * The plugin windows are tested only when MDAW_TEST_GUI=1 and DISPLAY is set; run that under a
 * virtual display, never the desktop's: test/run_gui_tests.sh does. */
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE
#include "mdaw_engine.h"
#include "mdaw_engine_test.h"

#include <dirent.h>
#include <pthread.h>
#include <errno.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef MDAW_TEST_SCANNER
#define MDAW_TEST_SCANNER ""
#endif
#ifndef MDAW_TEST_CLAP_DIR
#define MDAW_TEST_CLAP_DIR ""
#endif

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

/* The number value of "key" at or after `from`; NAN if there is none. */
static double realValue (const char* from, const char* key)
{
    char quoted[64];
    snprintf (quoted, sizeof quoted, "\"%s\"", key);
    const char* p = strstr (from, quoted);
    if (p == NULL) return NAN;
    p += strlen (quoted);
    while (*p == ' ' || *p == ':') ++p;
    char* end = NULL;
    double v = strtod (p, &end);
    return end == p ? NAN : v;
}

/* The object of the parameter with this id in a parameter list, copied into out; 0 if none. */
static int paramObject (const char* json, const char* id, char* out, size_t capacity)
{
    char wanted[96];
    snprintf (wanted, sizeof wanted, "\"id\": \"%s\"", id);
    const char* p = strstr (json, wanted);
    if (p == NULL) return 0;
    while (p > json && *p != '{') --p;
    const char* end = strchr (p, '}');
    if (end == NULL) return 0;
    size_t n = (size_t) (end - p + 1);
    if (n + 1 > capacity) return 0;
    memcpy (out, p, n);
    out[n] = 0;
    return 1;
}

/* How often `needle` occurs in `text`. */
static int occurrences (const char* text, const char* needle)
{
    int n = 0;
    for (const char* p = strstr (text, needle); p != NULL; p = strstr (p + 1, needle)) ++n;
    return n;
}

static int copyFile (const char* from, const char* to)
{
    FILE* in = fopen (from, "rb");
    if (in == NULL) return 0;
    FILE* out = fopen (to, "wb");
    if (out == NULL) { fclose (in); return 0; }
    char block[65536];
    size_t n;
    int ok = 1;
    while ((n = fread (block, 1, sizeof block, in)) > 0) ok &= fwrite (block, 1, n, out) == n;
    fclose (in);
    return fclose (out) == 0 && ok;
}

static char* paramText (mdaw_engine* e, uint64_t device, const char* param, float value)
{
    size_t n = mdaw_device_param_text (e, device, param, value, NULL, 0);
    char* text = malloc (n + 1);
    text[0] = 0;
    mdaw_device_param_text (e, device, param, value, text, n + 1);
    return text;
}

#define TEST_SYNTH_ID "org.mdaw.test-synth"

/* The identifier of the plugin named `name` in a plugin list, copied into out. */
static void identifierOf (const char* list, const char* name, char* out, size_t capacity)
{
    char quoted[256];
    snprintf (quoted, sizeof quoted, "\"%s\"", name);
    out[0] = 0;
    const char* at = strstr (list, quoted);
    if (at == NULL) return;
    while (at > list && *at != '{') --at;
    stringValue (at, "identifier", out, capacity);
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

static mdaw_engine* createWith (const char* settingsDir, const char* scanner)
{
    mdaw_engine_config config;
    memset (&config, 0, sizeof config);
    config.abi_version = MDAW_ABI_VERSION;
    config.app_name = "mdaw-bridge-test";
    config.settings_dir = settingsDir;
    config.open_audio_device = 0;
    config.audio_backend = NULL;
    config.enable_plugins = 1;
    config.scanner_path = scanner;
    return mdaw_engine_create (&config);
}

static mdaw_engine* create (void) { return createWith (SCRATCH "/settings", NULL); }

/* After a restart, with no scan: the test synth is listed under the same identifier and loads. */
static void restartedPluginList (mdaw_engine* e, int haveExternal)
{
    if (! haveExternal) return;
    char* list = pluginsJson (e);
    char identifier[1024] = "";
    identifierOf (list, "mdaw Test Synth", identifier, sizeof identifier);
    printf ("plugins after restart: %s\n", list);
    check (strcmp (identifier, TEST_SYNTH_ID) == 0 && occurrences (list, "\"" TEST_SYNTH_ID "\"") == 1, "restart_same_identifier");
    free (list);
    mdaw_note extNotes[8];
    quarterNotes (extNotes, 2000);
    mdaw_status s = mdaw_track_add (e, EXT_TRACK, TRACK, "External");
    s |= mdaw_device_add (e, EXT_TRACK, EXT, 0, "CLAP", TEST_SYNTH_ID);
    if (s != MDAW_OK) printLastError ("device_add after restart");
    s |= mdaw_clip_add (e, EXT_TRACK, EXT_CLIP, 0, 8 * Q, "Ext");
    s |= mdaw_notes_put (e, EXT_CLIP, extNotes, 8);
    mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 1);
    Wav w = render (e, "external_after_restart", 0, 8 * Q);
    measure ("external_after_restart_peak", peakOf (&w));
    check (s == MDAW_OK && peakOf (&w) > 0.01, "restart_plugin_loads");
    freeWav (&w);
}

/* -------- the plugin scanner ------------------------------------------------------------ */

static int cmp (const void* a, const void* b)
{
    double x = *(const double*) a, y = *(const double*) b;
    return x < y ? -1 : x > y ? 1 : 0;
}

static double now (void)
{
    struct timespec t;
    clock_gettime (CLOCK_MONOTONIC, &t);
    return (double) t.tv_sec + (double) t.tv_nsec / 1.0e9;
}

static char* failedJson (mdaw_engine* e)
{
    size_t n = mdaw_plugins_failed (e, NULL, 0);
    char* text = malloc (n + 1);
    text[0] = 0;
    mdaw_plugins_failed (e, text, n + 1);
    return text;
}

/* Whether a file is mapped into this process (a plugin library loaded). */
static int mapped (const char* path)
{
    FILE* f = fopen ("/proc/self/maps", "r");
    if (f == NULL) return -1;
    char line[4096];
    int found = 0;
    while (! found && fgets (line, sizeof line, f) != NULL)
        found = strstr (line, path) != NULL;
    fclose (f);
    return found;
}

/* The reason a failed list gives for a file; empty if the file is not in it. */
static void reasonFor (const char* json, const char* file, char* out, size_t capacity)
{
    char quoted[1200];
    snprintf (quoted, sizeof quoted, "\"%s\"", file);
    out[0] = 0;
    const char* at = strstr (json, quoted);
    if (at == NULL) return;
    while (at > json && *at != '{') --at;
    stringValue (at, "reason", out, capacity);
}

/* No child process of this one, running or unreaped. */
static int noChildren (void)
{
    errno = 0;
    return waitpid (-1, NULL, WNOHANG) == -1 && errno == ECHILD;
}

static void emptyFolder (const char* path)
{
    DIR* dir = opendir (path);
    if (dir == NULL) return;
    struct dirent* entry;
    while ((entry = readdir (dir)) != NULL)
    {
        if (entry->d_name[0] == '.') continue;
        char file[1200];
        snprintf (file, sizeof file, "%s/%s", path, entry->d_name);
        remove (file);
    }
    closedir (dir);
}

static double timedScan (mdaw_engine* e, mdaw_status* status)
{
    double start = now();
    *status = mdaw_plugins_scan (e, "CLAP");
    if (*status != MDAW_OK) printLastError ("scan with the scanner");
    return now() - start;
}

static void touchLater (const char* path, int seconds)
{
    struct timeval times[2];
    gettimeofday (&times[0], NULL);
    times[0].tv_sec += seconds;
    times[1] = times[0];
    utimes (path, times);
}

/* A scan on a thread of its own. */
typedef struct
{
    mdaw_engine* engine;
    volatile int done;
    double seconds;
    mdaw_status status;
} ScanJob;

static void* scanThread (void* arg)
{
    ScanJob* job = arg;
    double start = now();
    job->status = mdaw_plugins_scan (job->engine, "CLAP");
    job->seconds = now() - start;
    __atomic_store_n (&job->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

static int scanDone (ScanJob* job) { return __atomic_load_n (&job->done, __ATOMIC_SEQ_CST); }

/* The FILE line of the scan records (settings folder) for a file; empty if none. */
static void recordLine (const char* settings, const char* file, char* out, size_t capacity)
{
    char path[1200], line[8192];
    snprintf (path, sizeof path, "%s/plugin-scan.xml", settings);
    out[0] = 0;
    FILE* f = fopen (path, "r");
    if (f == NULL) return;
    while (fgets (line, sizeof line, f) != NULL)
        if (strstr (line, file) != NULL) { snprintf (out, capacity, "%s", line); break; }
    fclose (f);
}

/* -------- plugin windows (on a virtual display) ------------------------------------------ */

/* Waits ms milliseconds while reading events; returns how many EDITOR_CLOSED events for the
 * device came, and the token of the last one. Also notes whether one carried `unwanted`. */
static int editorClosedEvents (mdaw_engine* e, uint64_t device, int ms, uint64_t* lastToken, uint64_t unwanted, int* sawUnwanted)
{
    int n = 0;
    for (int waited = 0; waited <= ms; waited += 20)
    {
        mdaw_event ev;
        while (mdaw_engine_poll_event (e, &ev))
            if (ev.kind == MDAW_EVENT_EDITOR_CLOSED && ev.device == device)
            {
                printf ("event editor closed device=%llu token=%llu\n", (unsigned long long) ev.device, (unsigned long long) ev.token);
                ++n;
                if (lastToken != NULL) *lastToken = ev.token;
                if (sawUnwanted != NULL && ev.token == unwanted) *sawUnwanted = 1;
            }
        if (waited < ms) sleepMs (20);
    }
    return n;
}

static int windowOf (mdaw_engine* e, uint64_t device, int32_t* visible, uint64_t* token)
{
    *visible = -1;
    *token = 0;
    return mdaw_test_editor_state (e, device, visible, token);
}

static void editorChecks (const char* scanner)
{
    mdaw_engine* e = createWith (SCRATCH "/settings-scanner", scanner);
    check (e != NULL, "editor_engine_create");
    if (e == NULL) { printLastError ("create for the editor checks"); return; }
    enum { T = 60, A = 61, B = 62 };
    mdaw_status s = mdaw_track_add (e, T, 0, "Windows");
    s |= mdaw_device_add (e, T, A, 0, "CLAP", "org.mdaw.test-gui");
    s |= mdaw_device_add (e, T, B, A, "CLAP", "org.mdaw.test-gui");
    if (s != MDAW_OK) printLastError ("add the gui plugins");
    check (s == MDAW_OK, "editor_devices_added");
    int32_t visible;
    uint64_t token, last;
    int unwanted;

    check (mdaw_device_show_editor (e, A, 1, 1) == MDAW_OK && windowOf (e, A, &visible, &token) == 1 && visible == 1 && token == 1,
           "editor_open");
    check (mdaw_device_show_editor (e, A, 1, 2) == MDAW_OK && windowOf (e, A, &visible, &token) == 1 && visible == 1 && token == 2,
           "editor_open_again_one_window_new_token");
    check (mdaw_device_show_editor (e, A, 0, 0) == MDAW_OK && windowOf (e, A, &visible, &token) == 0, "editor_close");
    check (mdaw_device_show_editor (e, A, 0, 0) == MDAW_OK && windowOf (e, A, &visible, &token) == 0, "editor_close_again");
    check (editorClosedEvents (e, A, 200, NULL, 0, NULL) == 0, "editor_close_sends_no_event");

    /* The plugin failing to make its window. */
    setenv ("MDAW_TEST_GUI_FAIL", "1", 1);
    s = mdaw_device_show_editor (e, A, 1, 3);
    unsetenv ("MDAW_TEST_GUI_FAIL");
    check (s == MDAW_ERR_PLUGIN && windowOf (e, A, &visible, &token) == 0, "editor_gui_fail_plugin_no_window");
    check (mdaw_device_show_editor (e, A, 1, 4) == MDAW_OK && windowOf (e, A, &visible, &token) == 1 && token == 4,
           "editor_open_after_fail");

    /* Closed by the user: the timer deletes it and says so, with its token. */
    check (mdaw_test_editor_press_close (e, A) == MDAW_OK, "editor_press_close");
    last = 0;
    int closed = editorClosedEvents (e, A, 300, &last, 0, NULL);
    check (closed == 1 && last == 4 && windowOf (e, A, &visible, &token) == 0, "editor_user_close_event_with_token");
    check (mdaw_test_editor_press_close (e, A) == MDAW_ERR_NOT_FOUND, "editor_press_close_without_window");

    /* Closed and opened again at once, many times: the window that was opened stays. */
    check (mdaw_device_show_editor (e, A, 1, 100) == MDAW_OK, "editor_race_first_open");
    for (int i = 1; i <= 20; ++i)
    {
        mdaw_test_editor_press_close (e, A);
        mdaw_device_show_editor (e, A, 1, 100 + (uint64_t) i);
    }
    unwanted = 0;
    editorClosedEvents (e, A, 300, NULL, 120, &unwanted);
    check (windowOf (e, A, &visible, &token) == 1 && visible == 1 && token == 120 && ! unwanted, "editor_close_reopen_race");

    /* Two windows; closing one leaves the other. */
    check (mdaw_device_show_editor (e, B, 1, 200) == MDAW_OK, "editor_second_device_open");
    mdaw_test_editor_press_close (e, A);
    last = 0;
    closed = editorClosedEvents (e, A, 300, &last, 0, NULL);
    check (closed == 1 && last == 120 && windowOf (e, A, &visible, &token) == 0 && windowOf (e, B, &visible, &token) == 1
           && token == 200 && visible == 1, "editor_two_windows_close_one");

    /* The device removed with its window open. */
    check (mdaw_device_remove (e, B) == MDAW_OK, "editor_device_removed");
    last = 0;
    closed = editorClosedEvents (e, B, 100, &last, 0, NULL);
    check (closed == 1 && last == 200 && windowOf (e, B, &visible, &token) == 0, "editor_device_remove_event");

    /* The project cleared with a window open. */
    check (mdaw_device_show_editor (e, A, 1, 300) == MDAW_OK, "editor_open_before_clear");
    check (mdaw_project_clear (e) == MDAW_OK, "editor_project_clear");
    last = 0;
    closed = editorClosedEvents (e, A, 100, &last, 0, NULL);
    check (closed == 1 && last == 300 && windowOf (e, A, &visible, &token) == 0, "editor_project_clear_event");

    /* A plugin slow to make its window. */
    s = mdaw_track_add (e, T, 0, "Windows");
    s |= mdaw_device_add (e, T, A, 0, "CLAP", "org.mdaw.test-gui");
    setenv ("MDAW_TEST_GUI_DELAY_MS", "300", 1);
    double start = now();
    s |= mdaw_device_show_editor (e, A, 1, 400);
    double took = now() - start;
    unsetenv ("MDAW_TEST_GUI_DELAY_MS");
    measure ("editor_slow_open_seconds", took);
    check (s == MDAW_OK && took >= 0.3 && windowOf (e, A, &visible, &token) == 1 && token == 400, "editor_slow_open");
    mdaw_device_show_editor (e, A, 0, 0);

    mdaw_engine_destroy (e);
}

/* Good, crashing, hanging and stateless plugins in one CLAP folder, scanned with the scanner.
 * The machine's own CLAP folders are scanned as well (the format always looks there). */
static void scannerChecks (int haveSynth)
{
    const char* scanner = getenv ("MDAW_TEST_SCANNER");
    if (scanner == NULL || *scanner == 0) scanner = MDAW_TEST_SCANNER;
    const char* testPlugins = getenv ("MDAW_TEST_CLAP_DIR");
    if (testPlugins == NULL || *testPlugins == 0) testPlugins = MDAW_TEST_CLAP_DIR;
    const char* clapPath = getenv ("CLAP_PATH");
    printf ("scanner: %s, test plugins: %s\n", scanner, testPlugins);

    /* The byte order of VST3 class ids, checked by the scanner against the VST3 SDK. */
    {
        char command[1400];
        snprintf (command, sizeof command, "'%s' --self-test > " SCRATCH "/scanner-self-test.txt 2>&1", scanner);
        check (system (command) == 0, "vst3_id_byte_order");
    }

    char cwd[1024], dir[1200], crash[1300], hang[1300], stateless[1300], synth[1300], gui[1300], from[1300];
    if (getcwd (cwd, sizeof cwd) == NULL) cwd[0] = 0;
    snprintf (dir, sizeof dir, "%s/" SCRATCH "/scan-clap", cwd);
    mkdir (dir, 0755);
    emptyFolder (dir);
    mkdir (SCRATCH "/settings-scanner", 0755);
    emptyFolder (SCRATCH "/settings-scanner");
    snprintf (crash, sizeof crash, "%s/bad-crash.clap", dir);
    snprintf (hang, sizeof hang, "%s/bad-hang.clap", dir);
    snprintf (stateless, sizeof stateless, "%s/test-stateless.clap", dir);
    snprintf (synth, sizeof synth, "%s/mdaw-test-synth.clap", dir);
    snprintf (gui, sizeof gui, "%s/test-gui.clap", dir);
    int copied = 1;
    snprintf (from, sizeof from, "%s/test-gui.clap", testPlugins);
    copied &= copyFile (from, gui);
    snprintf (from, sizeof from, "%s/bad-crash.clap", testPlugins);
    copied &= copyFile (from, crash);
    snprintf (from, sizeof from, "%s/bad-hang.clap", testPlugins);
    copied &= copyFile (from, hang);
    snprintf (from, sizeof from, "%s/test-stateless.clap", testPlugins);
    copied &= copyFile (from, stateless);
    if (haveSynth && clapPath != NULL)
    {
        snprintf (from, sizeof from, "%s/mdaw-test-synth.clap", clapPath);
        haveSynth = copyFile (from, synth);
    }
    check (copied, "scanner_test_plugins_present");
    setenv ("CLAP_PATH", dir, 1);
    setenv ("MDAW_SCAN_TIMEOUT_MS", "3000", 1);

    mdaw_engine* e = createWith (SCRATCH "/settings-scanner", scanner);
    check (e != NULL, "scanner_engine_create");
    if (e == NULL) { printLastError ("create with the scanner"); return; }

    mdaw_status s;
    double first = timedScan (e, &s);
    measure ("scanner_first_scan_seconds", first);
    check (s == MDAW_OK, "scanner_scan_ok");
    char* list = pluginsJson (e);
    char* failed = failedJson (e);
    printf ("plugins (scanner): %s\nfailed (scanner): %s\n", list, failed);
    char crashReason[1024], hangReason[1024];
    reasonFor (failed, crash, crashReason, sizeof crashReason);
    reasonFor (failed, hang, hangReason, sizeof hangReason);
    printf ("crash: %s\nhang: %s\n", crashReason, hangReason);
    check (strstr (crashReason, "crashed") != NULL && strstr (crashReason, "signal 6") != NULL, "scanner_crash_listed_as_crash");
    check (strstr (hangReason, "hung") != NULL && strstr (hangReason, "3000 ms") != NULL, "scanner_hang_listed_as_hang");
    check (strstr (list, "org.mdaw.test-bad") == NULL, "scanner_bad_plugins_not_offered");
    check (strstr (list, "\"org.mdaw.test-stateless\"") != NULL, "scanner_stateless_listed");
    if (haveSynth)
        check (occurrences (list, "\"" TEST_SYNTH_ID "\"") == 1, "scanner_synth_listed_by_id");
    /* Nothing was loaded here to scan or to list: the scanner did it. */
    check (mapped (crash) == 0 && mapped (hang) == 0 && mapped (stateless) == 0 && mapped (synth) == 0, "scanner_no_library_loaded_here");
    check (noChildren(), "scanner_children_reaped");
    check (system ("pgrep -f 'mdaw-plugin-scan CLAP .*/bad[-]' > /dev/null") != 0, "scanner_no_leftover_processes");
    free (list);
    free (failed);

    /* Nothing changed: nothing is examined again, the failures stay. */
    double second = timedScan (e, &s);
    measure ("scanner_second_scan_seconds", second);
    failed = failedJson (e);
    reasonFor (failed, hang, hangReason, sizeof hangReason);
    reasonFor (failed, crash, crashReason, sizeof crashReason);
    check (s == MDAW_OK && second < 1.0 && hangReason[0] != 0 && crashReason[0] != 0, "scanner_second_scan_skips_known");
    free (failed);

    /* A changed file is examined again: the hanging one takes the time limit once more. */
    {
        struct timeval times[2];
        gettimeofday (&times[0], NULL);
        times[0].tv_sec += 10;
        times[1] = times[0];
        utimes (hang, times);
    }
    double third = timedScan (e, &s);
    measure ("scanner_changed_scan_seconds", third);
    failed = failedJson (e);
    reasonFor (failed, hang, hangReason, sizeof hangReason);
    check (s == MDAW_OK && third > 2.5 && strstr (hangReason, "hung") != NULL, "scanner_changed_file_examined_again");
    free (failed);
    check (noChildren() && system ("pgrep -f 'mdaw-plugin-scan CLAP .*/bad[-]' > /dev/null") != 0, "scanner_children_reaped_after_kill");

    /* Adding a device loads the plugin, for real. A plugin without state resets to its
     * parameters' defaults. */
    {
        mdaw_status a = mdaw_track_add (e, 50, 0, "Scanned");
        a |= mdaw_device_add (e, 50, 51, 0, "CLAP", "org.mdaw.test-stateless");
        if (a != MDAW_OK) printLastError ("add the stateless plugin");
        check (a == MDAW_OK && mapped (stateless) == 1, "scanner_device_add_loads");
        float value = -1.0f;
        a = mdaw_device_set_param (e, 51, "0", 0.9f);
        a |= mdaw_device_get_param (e, 51, "0", &value);
        check (a == MDAW_OK && fabsf (value - 0.9f) < 0.01f && mdaw_device_get_state (e, 51, NULL, 0) == 0, "stateless_param_set_no_state");
        mdaw_status reset = mdaw_device_reset_state (e, 51);
        if (reset != MDAW_OK) printLastError ("reset the stateless plugin");
        mdaw_device_get_param (e, 51, "0", &value);
        measure ("stateless_param_after_reset", value);
        check (reset == MDAW_OK && fabsf (value - 0.5f) < 0.01f && mdaw_project_has (e, MDAW_ENTITY_DEVICE, 51) == 1, "stateless_reset_to_defaults");

        /* A plugin without a window of its own: no editor, before any window is made. */
        check (mdaw_device_show_editor (e, 51, 1, 1) == MDAW_ERR_NOT_FOUND, "show_editor_no_gui_not_found");
    }

    /* One state read per round trip: a call with a buffer too small keeps what it read, and
     * the next one with a buffer that fits returns it without asking the plugin again. */
    {
        mdaw_status a = mdaw_device_add (e, 50, 55, 51, "CLAP", "org.mdaw.test-gui");
        if (a != MDAW_OK) printLastError ("add the gui plugin");
        check (a == MDAW_OK, "gui_plugin_added");
        uint8_t reference[256], small[1], fitting[256];
        size_t n = mdaw_device_get_state (e, 55, reference, sizeof reference);
        setenv ("MDAW_TEST_STATE_DELAY_MS", "300", 1);
        double start = now();
        size_t first = mdaw_device_get_state (e, 55, small, sizeof small);
        size_t second = first <= sizeof fitting ? mdaw_device_get_state (e, 55, fitting, sizeof fitting) : 0;
        double both = now() - start;
        unsetenv ("MDAW_TEST_STATE_DELAY_MS");
        measure ("state_round_trip_seconds", both);
        check (n > 1 && n <= sizeof reference && first == n && second == n && memcmp (reference, fitting, n) == 0,
               "state_round_trip_same_bytes");
        check (both >= 0.3 && both < 0.55, "state_round_trip_one_read");
        /* Kept only for the next call: another call in between drops it, and the plugin is
         * asked again. */
        setenv ("MDAW_TEST_STATE_DELAY_MS", "300", 1);
        start = now();
        first = mdaw_device_get_state (e, 55, small, sizeof small);
        (void) mdaw_project_has (e, MDAW_ENTITY_DEVICE, 55);
        second = mdaw_device_get_state (e, 55, fitting, sizeof fitting);
        both = now() - start;
        unsetenv ("MDAW_TEST_STATE_DELAY_MS");
        check (second == n && both >= 0.6, "state_kept_only_for_next_call");

        /* A state read at once after a set holds the value set (the plugin saves its Level as
         * 8 raw bytes), and the value stays: it is not taken back from the plugin. Before the
         * bridge handed sets over first, the state had the old Level and the parameter went
         * back to it, reported as the plugin's own change. */
        drainEvents (e);
        int fresh = 1, kept = 1;
        for (int round = 0; round < 5; ++round)
        {
            const double wanted = round % 2 == 0 ? 0.2 : 0.7;
            double saved = -1.0;
            mdaw_device_set_param (e, 55, "0", (float) wanted);
            size_t got = mdaw_device_get_state (e, 55, fitting, sizeof fitting);
            if (got == sizeof saved) memcpy (&saved, fitting, sizeof saved);
            if (fabs (saved - wanted) > 1e-3) { fresh = 0; printf ("round %d: state holds %g, set %g\n", round, saved, wanted); }
            sleepMs (100);
            float value = -1.0f;
            mdaw_device_get_param (e, 55, "0", &value);
            if (fabsf (value - (float) wanted) > 1e-3f) { kept = 0; printf ("round %d: parameter %g after the read, set %g\n", round, value, wanted); }
        }
        sleepMs (400);
        int events = drainEvents (e);
        check (fresh, "state_read_right_after_set_has_it");
        check (kept, "state_read_right_after_set_keeps_value");
        check (events == 0, "state_read_right_after_set_no_events");
        /* And restoring that state brings the value back. */
        {
            mdaw_device_set_param (e, 55, "0", 0.35f);
            size_t got = mdaw_device_get_state (e, 55, fitting, sizeof fitting);
            mdaw_device_set_param (e, 55, "0", 0.9f);
            mdaw_status r = mdaw_device_set_state (e, 55, fitting, got);
            float value = -1.0f;
            mdaw_device_get_param (e, 55, "0", &value);
            sleepMs (100);
            float later = -1.0f;
            mdaw_device_get_param (e, 55, "0", &later);
            measure ("state_restore_after_set_value", value);
            check (r == MDAW_OK && fabsf (value - 0.35f) < 1e-3f && fabsf (later - 0.35f) < 1e-3f, "state_restore_after_set_has_it");
        }
        check (mdaw_device_remove (e, 55) == MDAW_OK, "gui_plugin_removed");
    }

    /* A scan does not hold the engine's thread while the scanner examines a file: other calls
     * are answered, a known plugin is added and removed, and the timer keeps the position
     * fresh. */
    {
        touchLater (hang, 20);
        ScanJob job = { e, 0, 0.0, MDAW_OK };
        pthread_t t;
        pthread_create (&t, NULL, scanThread, &job);
        sleepMs (300);
        double worstHas = 0.0, worstAge = 0.0;
        int during = 1, addOk = 1, samples = 0;
        double start = now();
        int added = mdaw_device_add (e, 50, 56, 51, "CLAP", "org.mdaw.test-stateless") == MDAW_OK
                    && mdaw_project_has (e, MDAW_ENTITY_DEVICE, 56) == 1;
        int removed = mdaw_device_remove (e, 56) == MDAW_OK && mdaw_project_has (e, MDAW_ENTITY_DEVICE, 56) == 0;
        double addRemove = now() - start;
        addOk = added && removed;
        during = ! scanDone (&job);
        while (! scanDone (&job))
        {
            double t0 = now();
            (void) mdaw_project_has (e, MDAW_ENTITY_TRACK, 50);
            double took = now() - t0;
            if (took > worstHas) worstHas = took;
            mdaw_engine_state st;
            mdaw_engine_state_read (e, &st);
            if (st.position_age_seconds > worstAge) worstAge = st.position_age_seconds;
            ++samples;
            sleepMs (50);
        }
        pthread_join (t, NULL);
        measure ("scan_beside_seconds", job.seconds);
        measure ("scan_beside_worst_has_seconds", worstHas);
        measure ("scan_beside_worst_position_age", worstAge);
        measure ("scan_beside_add_remove_seconds", addRemove);
        measure ("scan_beside_samples", samples);
        check (during && samples > 10, "scan_beside_ran_during_scan");
        check (job.status == MDAW_OK && job.seconds >= 3.0, "scan_beside_took_the_time_limit");
        check (worstHas < 1.0, "scan_beside_calls_answered");
        check (addOk, "scan_beside_device_add_remove");
        check (worstAge < 1.0, "scan_beside_timer_runs");
        failed = failedJson (e);
        reasonFor (failed, hang, hangReason, sizeof hangReason);
        check (strstr (hangReason, "hung") != NULL, "scan_beside_hang_recorded");
        free (failed);
        check (noChildren(), "scan_beside_children_reaped");
    }
    if (haveSynth)
    {
        mdaw_note n[8];
        quarterNotes (n, 3000);
        mdaw_status a = mdaw_track_add (e, 54, 50, "Scanned synth");
        a |= mdaw_device_add (e, 54, 52, 0, "CLAP", TEST_SYNTH_ID);
        a |= mdaw_clip_add (e, 54, 53, 0, 8 * Q, "Scanned");
        a |= mdaw_notes_put (e, 53, n, 8);
        Wav w = render (e, "scanned_synth", 0, 8 * Q);
        measure ("scanned_synth_peak", peakOf (&w));
        check (a == MDAW_OK && peakOf (&w) > 0.01 && mapped (synth) == 1, "scanner_synth_plays");
        freeWav (&w);
    }
    mdaw_engine_destroy (e);

    /* The failures are remembered across a restart, without a scan. */
    e = createWith (SCRATCH "/settings-scanner", scanner);
    if (e != NULL)
    {
        failed = failedJson (e);
        list = pluginsJson (e);
        reasonFor (failed, hang, hangReason, sizeof hangReason);
        reasonFor (failed, crash, crashReason, sizeof crashReason);
        check (hangReason[0] != 0 && crashReason[0] != 0, "scanner_failures_remembered");
        check (strstr (list, "\"org.mdaw.test-stateless\"") != NULL && (! haveSynth || strstr (list, "\"" TEST_SYNTH_ID "\"") != NULL),
               "scanner_list_after_restart");
        free (failed);
        free (list);
        mdaw_engine_destroy (e);
    }
    check (noChildren(), "scanner_no_children_at_end");

    /* Plugin windows, on a virtual display only. */
    const char* wantGui = getenv ("MDAW_TEST_GUI");
    const char* display = getenv ("DISPLAY");
    if (wantGui != NULL && strcmp (wantGui, "1") == 0 && display != NULL && *display != 0)
        editorChecks (scanner);
    else
        printf ("RESULT editor_checks_skipped=1\n");

    /* Cancel, last: it is for the engine's life. */
    e = createWith (SCRATCH "/settings-scanner", scanner);
    check (e != NULL, "cancel_engine_create");
    if (e == NULL) return;
    {
        touchLater (hang, 40);
        char before[8192], after[8192];
        recordLine (SCRATCH "/settings-scanner", hang, before, sizeof before);
        ScanJob job = { e, 0, 0.0, MDAW_OK };
        pthread_t t;
        pthread_create (&t, NULL, scanThread, &job);
        sleepMs (300);
        double cancelledAt = now();
        check (mdaw_plugins_scan_cancel (e) == MDAW_OK, "cancel_ok");
        while (! scanDone (&job) && now() - cancelledAt < 5.0) sleepMs (10);
        double took = now() - cancelledAt;
        pthread_join (t, NULL);
        measure ("cancel_return_seconds", took);
        measure ("cancelled_scan_seconds", job.seconds);
        check (job.status == MDAW_ERR_CANCELLED && took < 3.0, "cancel_returns_cancelled");
        recordLine (SCRATCH "/settings-scanner", hang, after, sizeof after);
        printf ("hang record before: %safter: %s", before, after);
        check (before[0] != 0 && strcmp (before, after) == 0, "cancel_leaves_record");
        check (noChildren() && system ("pgrep -f 'mdaw-plugin-scan CLAP .*/bad[-]' > /dev/null") != 0, "cancel_no_children");
        double start = now();
        mdaw_status again = mdaw_plugins_scan (e, "CLAP");
        check (again == MDAW_ERR_CANCELLED && now() - start < 0.5, "cancel_is_sticky");
        check (mdaw_plugins_scan_cancel (NULL) == MDAW_ERR_ARGUMENT, "cancel_null_engine");
    }
    mdaw_engine_destroy (e);
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
        check (strstr (versions, "Tracktion Engine v") != NULL && strstr (versions, "Tracktion Engine Tracktion Engine") == NULL
               && strstr (versions, "JUCE v") != NULL, "versions_text");
        check (mdaw_last_status() == MDAW_OK, "last_status_ok_before_any_failure");
        mdaw_engine* second = create();
        char error[512];
        size_t errorLength = mdaw_last_error (error, sizeof error);
        mdaw_status status = mdaw_last_status();
        printf ("second create: status %d, %s\n", (int) status, error);
        check (second == NULL && errorLength > 0, "second_create_refused");
        check (status == MDAW_ERR_INTERNAL, "second_create_last_status");
        if (second != NULL) mdaw_engine_destroy (second);
        mdaw_engine_config bad;
        memset (&bad, 0, sizeof bad);
        bad.abi_version = 1;
        bad.settings_dir = SCRATCH "/settings";
        check (mdaw_engine_create (&bad) == NULL && mdaw_last_status() == MDAW_ERR_ARGUMENT, "old_abi_refused_as_argument");
    }

    /* 2. A project, rendered --------------------------------------------------------------- */
    mdaw_note notes[8];
    quarterNotes (notes, 1000);
    build (e, notes);
    check (mdaw_project_has (e, MDAW_ENTITY_TRACK, TRACK) == 1 && mdaw_project_has (e, MDAW_ENTITY_CLIP, CLIP) == 1
           && mdaw_project_has (e, MDAW_ENTITY_DEVICE, SYNTH) == 1 && mdaw_project_has (e, MDAW_ENTITY_TEMPO, TEMPO0) == 1,
           "has_after_build");
    check (mdaw_project_has (e, MDAW_ENTITY_TRACK, CLIP) == 0 && mdaw_project_has (e, MDAW_ENTITY_DEVICE, TRACK) == 0
           && mdaw_project_has (e, MDAW_ENTITY_TEMPO, TEMPO2) == 0 && mdaw_project_has (e, MDAW_ENTITY_CLIP, 999) == 0,
           "has_not_for_other_kinds_or_ids");
    check (mdaw_project_has (e, 99, TRACK) == 0 && mdaw_last_status() == MDAW_ERR_ARGUMENT && mdaw_project_has (NULL, MDAW_ENTITY_TRACK, TRACK) == 0,
           "has_bad_arguments");
    check (mdaw_clip_set_name (e, CLIP, "Renamed clip") == MDAW_OK && mdaw_clip_set_name (e, 999, "x") == MDAW_OK
           && mdaw_clip_set_name (e, CLIP, NULL) == MDAW_OK, "clip_set_name");
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
        s |= mdaw_device_show_editor (e, 999, 0, 0);
        s |= mdaw_tempo_remove (e, 999);
        check (s == MDAW_OK, "missing_ids_ok");
        check (mdaw_device_show_editor (e, 999, 1, 1) == MDAW_ERR_NOT_FOUND, "show_editor_missing_device_not_found");
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
        {
            char tempoError[512];
            mdaw_status t = mdaw_tempo_put (e, 3, 0, 301000);
            mdaw_last_error (tempoError, sizeof tempoError);
            printf ("tempo error: %s\n", tempoError);
            check (t == MDAW_ERR_ARGUMENT && strstr (tempoError, "20 to 300") != NULL && mdaw_tempo_put (e, 3, Q, 300000) == MDAW_OK
                   && mdaw_tempo_remove (e, 3) == MDAW_OK, "tempo_range_says_range");
        }
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
        mdaw_engine_state_read (e, &st);
        measure ("position_age_seconds", st.position_age_seconds);
        check (st.position_age_seconds >= 0.0 && st.position_age_seconds < 1.0, "position_age");
        check (st.output_latency_seconds == 0.0, "no_output_latency_without_device");
    }

    /* 8b. Live notes (ABI 4). A render cannot hear them; without a device they are dropped. What
     * is checked: the status, that nothing breaks, and the cost of the call next to a call that
     * waits for the message thread. */
    {
        Wav w1 = render (e, "before_live_notes", 0, 8 * Q);
        check (mdaw_track_live_note (e, TRACK, 60, 100) == MDAW_OK && mdaw_track_live_note (e, TRACK, 60, 0) == MDAW_OK,
               "live_note_on_off");
        check (mdaw_track_live_note (e, 999, 60, 100) == MDAW_OK && mdaw_track_live_note (e, 999, 60, 0) == MDAW_OK,
               "live_note_unknown_track");
        check (mdaw_track_live_note (e, TRACK, 128, 100) == MDAW_ERR_ARGUMENT && mdaw_track_live_note (e, TRACK, 60, 128) == MDAW_ERR_ARGUMENT
               && mdaw_track_live_note (NULL, TRACK, 60, 100) == MDAW_ERR_ARGUMENT, "live_note_bad_arguments");

        enum { N = 400 };
        static double waits[N], notes[N];
        for (int i = 0; i < N; ++i)
        {
            double t0 = now();
            mdaw_project_has (e, MDAW_ENTITY_TRACK, TRACK);
            double t1 = now();
            mdaw_track_live_note (e, TRACK, (uint8_t) (40 + i % 40), (uint8_t) (i % 2 == 0 ? 90 : 0));
            double t2 = now();
            waits[i] = t1 - t0;
            notes[i] = t2 - t1;
            if (i % 50 == 0) sleepMs (3);
        }
        qsort (waits, N, sizeof (double), cmp);
        qsort (notes, N, sizeof (double), cmp);
        measure ("blocking_call_round_trip_p50_ms", waits[N / 2] * 1000.0);
        measure ("blocking_call_round_trip_p99_ms", waits[N * 99 / 100] * 1000.0);
        measure ("blocking_call_round_trip_max_ms", waits[N - 1] * 1000.0);
        measure ("live_note_call_p50_ms", notes[N / 2] * 1000.0);
        measure ("live_note_call_max_ms", notes[N - 1] * 1000.0);
        check (mdaw_project_has (e, MDAW_ENTITY_TRACK, TRACK) == 1, "engine_fine_after_live_notes");
        Wav w2 = render (e, "after_live_notes", 0, 8 * Q);
        check (w1.ok && w2.ok && fabs (peakOf (&w2) - peakOf (&w1)) < 0.5 * peakOf (&w1) + 0.001
               && fabs (lastAbove (&w2, 0.001) - lastAbove (&w1, 0.001)) < 0.1, "live_notes_leave_the_render_alone");
        freeWav (&w1);
        freeWav (&w2);
    }

    /* 9. Parameters and state of the built-in synth -------------------------------------- */
    {
        char* json = paramsJson (e, SYNTH);
        char param[128] = "";
        int found = continuousParam (json, param, sizeof param);
        measure ("synth_params_json_bytes", (double) strlen (json));
        check (json[0] == '[' && strlen (json) > 10 && found, "synth_params_listed");
        printf ("param used: %s\n", param);
        {
            char object[1024] = "";
            paramObject (json, param, object, sizeof object);
            printf ("builtin param: %s\n", object);
            char text[256] = "";
            check (realValue (object, "min") == 0.0 && realValue (object, "max") == 1.0
                   && stringValue (object, "text", text, sizeof text) != NULL && text[0] != 0, "builtin_param_min_max_text");
        }
        free (json);

        float initial = -1.0f;
        mdaw_device_get_param (e, SYNTH, param, &initial);
        {
            char* text = paramText (e, SYNTH, param, 0.3f);
            printf ("builtin param text at 0.3: %s\n", text);
            check (text[0] != 0, "builtin_param_text");
            free (text);
            char none[64];
            size_t n = mdaw_device_param_text (e, SYNTH, "no-such-param", 0.5f, none, sizeof none);
            check (n == 0 && mdaw_last_status() == MDAW_ERR_NOT_FOUND, "param_text_unknown_last_status_not_found");
            n = mdaw_device_param_text (e, 999, param, 0.5f, none, sizeof none);
            check (n == 0 && mdaw_last_status() == MDAW_ERR_NOT_FOUND, "param_text_missing_device_not_found");
            n = mdaw_device_param_text (e, SYNTH, param, 1.5f, none, sizeof none);
            check (n == 0 && mdaw_last_status() == MDAW_ERR_ARGUMENT, "param_text_out_of_range");
        }

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

        s = mdaw_device_reset_state (e, SYNTH);
        mdaw_device_get_param (e, SYNTH, param, &value);
        measure ("synth_param_initial", initial);
        measure ("synth_param_after_reset", value);
        check (s == MDAW_OK && fabsf (value - initial) < 0.01f && mdaw_project_has (e, MDAW_ENTITY_DEVICE, SYNTH) == 1, "synth_reset_state");
        check (mdaw_device_reset_state (e, 999) == MDAW_OK, "reset_missing_device_ok");
        w = render (e, "after_reset", 0, 8 * Q);
        check (peakOf (&w) > 0.05, "synth_renders_after_reset");
        freeWav (&w);

        /* 11. Our own changes are not reported back. */
        sleepMs (300);
        int events = drainEvents (e);
        measure ("events_after_own_changes", events);
        check (events == 0, "no_echo_events_builtin");
        check (mdaw_device_show_editor (e, SYNTH, 1, 1) == MDAW_ERR_NOT_FOUND, "builtin_has_no_editor");
    }

    /* 10. An external plugin -------------------------------------------------------------- */
    int haveExternal = 0;
    {
        check (mdaw_device_add (e, TRACK, 102, 0, "CLAP", "nonsense") == MDAW_ERR_NOT_FOUND, "unknown_plugin_not_found");
        char* nothing = paramsJson (e, 102);
        check (strcmp (nothing, "[]") == 0 && mdaw_device_get_state (e, 102, NULL, 0) == 0, "unknown_plugin_created_nothing");
        free (nothing);

        /* A second file with the same plugin: listed once. Removed after the scan, it stays in
         * the remembered list, which must not get in the way. */
        char original[1024] = "", copy[1024] = "";
        const char* clapPath = getenv ("CLAP_PATH");
        if (clapPath != NULL && strchr (clapPath, ':') == NULL)
        {
            snprintf (original, sizeof original, "%s/mdaw-test-synth.clap", clapPath);
            snprintf (copy, sizeof copy, "%s/mdaw-test-synth-copy.clap", clapPath);
            remove (copy);
            if (! copyFile (original, copy)) copy[0] = 0;
        }

        mdaw_status scan = mdaw_plugins_scan (e, "CLAP");
        if (scan != MDAW_OK) printLastError ("scan");
        check (scan == MDAW_OK, "scan_clap");
        char* list = pluginsJson (e);
        printf ("plugins: %s\n", list);
        char identifier[1024] = "";
        identifierOf (list, "mdaw Test Synth", identifier, sizeof identifier);
        if (copy[0] != 0)
        {
            check (occurrences (list, "\"" TEST_SYNTH_ID "\"") == 1, "same_plugin_twice_listed_once");
            remove (copy);
            free (list);
            list = pluginsJson (e);
            check (occurrences (list, "\"" TEST_SYNTH_ID "\"") == 1, "removed_file_still_listed_once");
        }
        free (list);

        haveExternal = identifier[0] != 0;
        if (identifier[0] == 0)
        {
            printf ("RESULT external_skipped=1\n");
        }
        else
        {
            printf ("external identifier: %s\n", identifier);
            check (strcmp (identifier, TEST_SYNTH_ID) == 0, "clap_identifier_is_clap_id");
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

            check (mdaw_project_has (e, MDAW_ENTITY_DEVICE, EXT) == 1, "external_has");
            char* json = paramsJson (e, EXT);
            char param[128] = "";
            int found = continuousParam (json, param, sizeof param);
            printf ("external params: %s\n", json);
            check (found, "external_params_listed");
            {
                char level[1024] = "", balance[1024] = "", text[256] = "";
                int both = paramObject (json, "0", level, sizeof level) && paramObject (json, "1", balance, sizeof balance);
                check (both && realValue (balance, "min") == -1.0 && realValue (balance, "max") == 1.0
                       && fabs (realValue (balance, "default") - 0.5) < 1e-6
                       && realValue (level, "min") == 0.0 && realValue (level, "max") == 1.0, "clap_param_ranges");
                check (stringValue (level, "text", text, sizeof text) != NULL && text[0] != 0, "clap_param_text_in_list");
                printf ("level text: %s\n", text);
                char* t = paramText (e, EXT, "0", 0.25f);
                printf ("level text at 0.25: %s\n", t);
                check (t[0] != 0 && strstr (t, "25") != NULL, "clap_param_text");
                free (t);
            }
            free (json);
            if (found)
            {
                float value = -1.0f;
                mdaw_device_set_param (e, EXT, param, 0.3f);
                mdaw_device_get_param (e, EXT, param, &value);
                measure ("external_param_read_back", value);
                check (fabsf (value - 0.3f) < 0.01f, "external_param_set_get");

                /* The state is read right after the set, with no block processed in between: it
                 * holds the value all the same, and restoring it brings the value back. */
                size_t n = mdaw_device_get_state (e, EXT, NULL, 0);
                uint8_t* state = malloc (n + 1);
                mdaw_device_get_state (e, EXT, state, n);
                measure ("external_state_bytes", (double) n);
                mdaw_device_set_param (e, EXT, param, 0.8f);
                mdaw_status r = mdaw_device_set_state (e, EXT, state, n);
                mdaw_device_get_param (e, EXT, param, &value);
                measure ("external_param_after_restore", value);
                check (r == MDAW_OK && fabsf (value - 0.3f) < 0.01f, "external_state_has_value_set_just_before");
                free (state);

                /* Back to the freshly loaded plugin: Level 0.5. Level 0.1 is rendered first so
                 * that it has reached the plugin (and its state). */
                mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 1);
                w = render (e, "external_before_quiet", 0, 8 * Q);
                double loud = peakOf (&w);
                freeWav (&w);
                mdaw_device_set_param (e, EXT, "0", 0.1f);
                w = render (e, "external_quiet", 0, 8 * Q);
                double quiet = peakOf (&w);
                freeWav (&w);
                mdaw_status reset = mdaw_device_reset_state (e, EXT);
                float level = -1.0f;
                mdaw_device_get_param (e, EXT, "0", &level);
                w = render (e, "external_reset", 0, 8 * Q);
                double afterReset = peakOf (&w);
                freeWav (&w);
                mdaw_track_set_mix (e, TRACK, 1.0f, 0.0f, 0);
                measure ("external_peak_before_quiet", loud);
                measure ("external_peak_quiet", quiet);
                measure ("external_level_after_reset", level);
                measure ("external_peak_after_reset", afterReset);
                check (reset == MDAW_OK && fabsf (level - 0.5f) < 0.01f && mdaw_project_has (e, MDAW_ENTITY_DEVICE, EXT) == 1, "external_reset_state");
                check (quiet < 0.5 * afterReset && afterReset > 0.01, "external_reset_state_audible");

                sleepMs (400);
                int events = drainEvents (e);
                measure ("events_after_own_external_changes", events);
                check (events == 0, "no_echo_events_external");
            }
            /* Reported only: whether the test synth has a window depends on the plugin. */
            mdaw_status editor = mdaw_device_show_editor (e, EXT, 1, 1);
            printf ("RESULT external_editor_status=%d\n", (int) editor);
            if (editor == MDAW_OK) mdaw_device_show_editor (e, EXT, 0, 0);
            check (mdaw_device_remove (e, EXT) == MDAW_OK && mdaw_project_has (e, MDAW_ENTITY_DEVICE, EXT) == 0, "external_removed");
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
        check (mdaw_project_has (e, MDAW_ENTITY_TRACK, TRACK) == 0 && mdaw_project_has (e, MDAW_ENTITY_CLIP, CLIP) == 0
               && mdaw_project_has (e, MDAW_ENTITY_DEVICE, SYNTH) == 0, "has_after_track_remove");
        check (mdaw_project_has (e, MDAW_ENTITY_TEMPO, TEMPO0) == 0, "has_tempo_after_remove");
        mdaw_tempo_put (e, TEMPO0, 0, 120000);
        check (mdaw_project_clear (e) == MDAW_OK, "project_clear");
        check (mdaw_project_has (e, MDAW_ENTITY_TEMPO, TEMPO0) == 0, "has_after_clear");
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
        restartedPluginList (e, haveExternal);
        mdaw_engine_destroy (e);
        check (1, "second_life_destroyed");
    }

    /* 13. A third life without the remembered ids: they are read from the plugins again. */
    if (haveExternal)
    {
        remove (SCRATCH "/settings/plugin-ids.xml");
        e = create();
        check (e != NULL, "third_life_create");
        if (e != NULL)
        {
            build (e, notes);
            restartedPluginList (e, haveExternal);
            mdaw_engine_destroy (e);
        }
    }

    /* 14. Plugins examined by the scanner, in child processes ---------------------------- */
    scannerChecks (haveExternal);

    printf ("RESULT failures=%d\n", failures);
    printf ("RESULT ok=%d\n", failures == 0 ? 1 : 0);
    return failures == 0 ? 0 : 1;
}
