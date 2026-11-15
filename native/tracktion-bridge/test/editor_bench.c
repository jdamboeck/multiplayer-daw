/* Times what stands behind a plugin's window, through mdaw_engine.h alone (ABI 5), so that the
 * same program measures the bridge before and after a change:
 *   - scanning each format (cold on a fresh settings folder, cached on a second run);
 *   - per plugin: adding the device, opening, closing and reopening its editor, reading its state;
 *   - how the engine's thread answers while a scan examines a plugin that hangs.
 * Output: `bench <name> <value> <unit>` lines and `#` comments. Editor windows need an X display:
 * run it under `xvfb-run -a` (test/run_editor_bench.sh does).
 *
 *   mdaw-editor-bench <settings-dir> [FORMAT:ID[@label] ...]
 *
 * CLAP:org.mdaw.test-gui (test/bad_clap.c, built next to this program) is always measured; it must
 * be in a folder on CLAP_PATH. The first folder of CLAP_PATH also gets bad-hang.clap for the scan
 * part, which is removed again at the end. The scanner and the test plugins are found where CMake
 * built them (MDAW_TEST_SCANNER, MDAW_TEST_CLAP_DIR, compiled in; environment variables of the same
 * names override them). */
#define _POSIX_C_SOURCE 200809L
#define _DEFAULT_SOURCE
#include "mdaw_engine.h"

#include <ctype.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#ifndef MDAW_TEST_SCANNER
#define MDAW_TEST_SCANNER ""
#endif
#ifndef MDAW_TEST_CLAP_DIR
#define MDAW_TEST_CLAP_DIR ""
#endif

#define TRACK 1
#define GUI_ID "org.mdaw.test-gui"
#define HANG_TIMEOUT_MS "4000"

static double nowMs (void)
{
    struct timespec t;
    clock_gettime (CLOCK_MONOTONIC, &t);
    return (double) t.tv_sec * 1000.0 + (double) t.tv_nsec / 1.0e6;
}

static void sleepMs (int ms)
{
    struct timespec t = { ms / 1000, (long) (ms % 1000) * 1000000L };
    nanosleep (&t, NULL);
}

static void bench (const char* name, double value, const char* unit)
{
    printf ("bench %s %.1f %s\n", name, value, unit);
    fflush (stdout);
}

static void reportError (const char* what, mdaw_status status)
{
    char text[1024] = "";
    mdaw_last_error (text, sizeof text);
    printf ("# %s failed: status %d: %s\n", what, (int) status, text);
    fflush (stdout);
}

static const char* envOr (const char* name, const char* fallback)
{
    const char* value = getenv (name);
    return value != NULL && *value != 0 ? value : fallback;
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

static char* listJson (mdaw_engine* e, size_t (*fn) (mdaw_engine*, char*, size_t))
{
    size_t n = fn (e, NULL, 0);
    char* text = malloc (n + 1);
    text[0] = 0;
    fn (e, text, n + 1);
    return text;
}

/* "vst3_0123abcd" from ("VST3", "0123abcd…"): the format in lower case and up to 16 characters
 * of the id's last part, letters and digits only. */
static void labelOf (const char* format, const char* id, char* out, size_t size)
{
    const char* tail = id;
    for (const char* c = id; *c != 0; ++c)
        if ((*c == '/' || *c == '#' || *c == ':' || *c == '.') && c[1] != 0) tail = c + 1;
    size_t n = 0;
    for (const char* c = format; *c != 0 && n + 1 < size; ++c) out[n++] = (char) tolower ((unsigned char) *c);
    if (n + 1 < size) out[n++] = '_';
    size_t taken = 0;
    for (const char* c = tail; *c != 0 && n + 1 < size && taken < 16; ++c)
        if (isalnum ((unsigned char) *c)) { out[n++] = (char) tolower ((unsigned char) *c); ++taken; }
    out[n] = 0;
}

/* -------- the scan running beside the engine's other callers ------------------------------ */

typedef struct
{
    mdaw_engine* engine;
    volatile int done;
    double ms;
    mdaw_status status;
} ScanJob;

static void* scanThread (void* arg)
{
    ScanJob* job = arg;
    double t = nowMs();
    job->status = mdaw_plugins_scan (job->engine, "CLAP");
    job->ms = nowMs() - t;
    __atomic_store_n (&job->done, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

typedef struct
{
    mdaw_engine* engine;
    ScanJob* scan;
    double maxAgeMs;
    int samples;
} Sampler;

/* position_age_seconds grows while the engine's thread does not get to its 30 Hz timer. */
static void* samplerThread (void* arg)
{
    Sampler* s = arg;
    while (! __atomic_load_n (&s->scan->done, __ATOMIC_SEQ_CST))
    {
        mdaw_engine_state st;
        memset (&st, 0, sizeof st);
        mdaw_engine_state_read (s->engine, &st);
        if (st.position_age_seconds * 1000.0 > s->maxAgeMs) s->maxAgeMs = st.position_age_seconds * 1000.0;
        ++s->samples;
        sleepMs (50);
    }
    return NULL;
}

typedef struct
{
    mdaw_engine* engine;
    int delayMs;
    double ms;
} HasCall;

static void* hasThread (void* arg)
{
    HasCall* c = arg;
    sleepMs (c->delayMs);
    double t = nowMs();
    (void) mdaw_project_has (c->engine, MDAW_ENTITY_TRACK, TRACK);
    c->ms = nowMs() - t;
    return NULL;
}

static double maxAgeOver (mdaw_engine* e, int ms)
{
    double worst = 0.0;
    for (int waited = 0; waited < ms; waited += 50)
    {
        mdaw_engine_state st;
        memset (&st, 0, sizeof st);
        mdaw_engine_state_read (e, &st);
        if (st.position_age_seconds * 1000.0 > worst) worst = st.position_age_seconds * 1000.0;
        sleepMs (50);
    }
    return worst;
}

/* -------- one plugin's device and window ---------------------------------------------------- */

static void measurePlugin (mdaw_engine* e, uint64_t device, const char* format, const char* id, const char* label)
{
    char name[128];
    printf ("# %s: %s %s (device %llu)\n", label, format, id, (unsigned long long) device);
    double t = nowMs();
    mdaw_status s = mdaw_device_add (e, TRACK, device, 0, format, id);
    double ms = nowMs() - t;
    if (s != MDAW_OK) { reportError ("device_add", s); return; }
    snprintf (name, sizeof name, "add_%s_ms", label);
    bench (name, ms, "ms");

    const char* steps[] = { "open", "close", "reopen", NULL };
    const int show[] = { 1, 0, 1 };
    for (int i = 0; steps[i] != NULL; ++i)
    {
        t = nowMs();
        s = mdaw_device_show_editor (e, device, show[i], (uint64_t) i + 1);
        ms = nowMs() - t;
        snprintf (name, sizeof name, "%s_%s_ms", steps[i], label);
        if (s != MDAW_OK) reportError (name, s);
        else bench (name, ms, "ms");
        if (show[i]) sleepMs (300);   /* let the window come up before it is closed */
    }
    t = nowMs();
    s = mdaw_device_show_editor (e, device, 0, 0);
    ms = nowMs() - t;
    if (s != MDAW_OK) reportError ("close again", s);
    else { snprintf (name, sizeof name, "close2_%s_ms", label); bench (name, ms, "ms"); }

    t = nowMs();
    size_t n = mdaw_device_get_state (e, device, NULL, 0);
    double first = nowMs() - t;
    uint8_t* buffer = malloc (n > 0 ? n : 1);
    t = nowMs();
    size_t again = mdaw_device_get_state (e, device, buffer, n);
    ms = nowMs() - t;
    free (buffer);
    printf ("# %s: state %zu bytes (then %zu), size query %.1f ms\n", label, n, again, first);
    snprintf (name, sizeof name, "get_state_%s_ms", label);
    bench (name, ms, "ms");
}

int main (int argc, char** argv)
{
    if (argc < 2)
    {
        fprintf (stderr, "usage: %s <settings-dir> [FORMAT:ID[@label] ...]\n", argv[0]);
        return 2;
    }
    const double started = nowMs();
    const char* settings = argv[1];
    const char* scanner = envOr ("MDAW_TEST_SCANNER", MDAW_TEST_SCANNER);
    const char* testPlugins = envOr ("MDAW_TEST_CLAP_DIR", MDAW_TEST_CLAP_DIR);

    /* The folder that gets the hanging plugin: the first one on CLAP_PATH. It is not there for
     * the plain scans, so they time the installed plugins alone. */
    char clapDir[4096] = "", hangCopy[4200] = "", hangFrom[4200] = "";
    const char* clapPath = getenv ("CLAP_PATH");
    if (clapPath != NULL)
    {
        size_t n = strcspn (clapPath, ":");
        if (n < sizeof clapDir) { memcpy (clapDir, clapPath, n); clapDir[n] = 0; }
    }
    if (clapDir[0] != 0)
    {
        snprintf (hangCopy, sizeof hangCopy, "%s/bad-hang.clap", clapDir);
        snprintf (hangFrom, sizeof hangFrom, "%s/bad-hang.clap", testPlugins);
        unlink (hangCopy);
    }
    printf ("# scanner %s, test plugins %s, CLAP_PATH %s\n", scanner, testPlugins, clapPath != NULL ? clapPath : "(unset)");
    /* Read by the engine when it scans; set before it starts, as nothing else reads it. */
    setenv ("MDAW_SCAN_TIMEOUT_MS", HANG_TIMEOUT_MS, 1);

    mdaw_engine_config config;
    memset (&config, 0, sizeof config);
    config.abi_version = MDAW_ABI_VERSION;
    config.app_name = "mdaw-editor-bench";
    config.settings_dir = settings;
    config.open_audio_device = 0;
    config.enable_plugins = 1;
    config.scanner_path = *scanner != 0 ? scanner : NULL;
    double t = nowMs();
    mdaw_engine* e = mdaw_engine_create (&config);
    bench ("engine_create_ms", nowMs() - t, "ms");
    if (e == NULL) { reportError ("engine_create", mdaw_last_status()); return 1; }

    const char* formats[] = { "CLAP", "VST3", "LV2" };
    for (int i = 0; i < 3; ++i)
    {
        char name[64];
        t = nowMs();
        mdaw_status s = mdaw_plugins_scan (e, formats[i]);
        double ms = nowMs() - t;
        snprintf (name, sizeof name, "scan_%s_ms", formats[i]);
        for (char* c = name; *c != 0; ++c) *c = (char) tolower ((unsigned char) *c);
        if (s != MDAW_OK) reportError (name, s);
        bench (name, ms, "ms");
    }
    char* list = listJson (e, mdaw_plugins_list);
    char* failed = listJson (e, mdaw_plugins_failed);
    printf ("# plugins: %s\n# failed: %s\n", list, failed);
    free (list);
    free (failed);

    mdaw_status s = mdaw_track_add (e, TRACK, 0, "Bench");
    if (s != MDAW_OK) reportError ("track_add", s);
    bench ("idle_max_position_age_ms", maxAgeOver (e, 500), "ms");

    const uint64_t guiDevice = 100;
    measurePlugin (e, guiDevice, "CLAP", GUI_ID, "clap_testgui");
    for (int i = 2; i < argc; ++i)
    {
        char arg[2048], label[64];
        snprintf (arg, sizeof arg, "%s", argv[i]);
        char* colon = strchr (arg, ':');
        if (colon == NULL) { printf ("# skipped %s: not FORMAT:ID\n", argv[i]); continue; }
        *colon = 0;
        char* id = colon + 1;
        char* at = strrchr (id, '@');
        if (at != NULL) { *at = 0; snprintf (label, sizeof label, "%s", at + 1); }
        else labelOf (arg, id, label, sizeof label);
        measurePlugin (e, 100 + (uint64_t) i, arg, id, label);
    }

    /* The engine's thread while a scan examines a plugin that hangs until the time limit. */
    if (hangCopy[0] != 0 && copyFile (hangFrom, hangCopy))
    {
        utimes (hangCopy, NULL);
        printf ("# scan with %s (time limit %s ms)\n", hangCopy, HANG_TIMEOUT_MS);
        ScanJob scan = { e, 0, 0.0, MDAW_OK };
        Sampler sampler = { e, &scan, 0.0, 0 };
        HasCall has = { e, 300, 0.0 };
        pthread_t scanT, samplerT, hasT;
        pthread_create (&scanT, NULL, scanThread, &scan);
        pthread_create (&samplerT, NULL, samplerThread, &sampler);
        pthread_create (&hasT, NULL, hasThread, &has);
        sleepMs (310);
        int scanDoneBeforeOpen = __atomic_load_n (&scan.done, __ATOMIC_SEQ_CST);
        t = nowMs();
        s = mdaw_device_show_editor (e, guiDevice, 1, 10);
        double openMs = nowMs() - t;
        pthread_join (hasT, NULL);
        pthread_join (scanT, NULL);
        pthread_join (samplerT, NULL);
        if (scan.status != MDAW_OK) reportError ("scan with the hanging plugin", scan.status);
        bench ("call_during_scan_ms", has.ms, "ms");
        if (s != MDAW_OK) reportError ("open_during_scan", s);
        else bench ("open_during_scan_ms", openMs, "ms");
        if (scanDoneBeforeOpen) printf ("# the scan was over before the editor was asked for\n");
        bench ("max_position_age_during_scan_ms", sampler.maxAgeMs, "ms");
        printf ("# %d samples of position_age\n", sampler.samples);
        bench ("hang_scan_ms", scan.ms, "ms");
        sleepMs (300);
        s = mdaw_device_show_editor (e, guiDevice, 0, 0);
        if (s != MDAW_OK) reportError ("close after the scan", s);
        unlink (hangCopy);
    }
    else
        printf ("# no scan with a hanging plugin: no CLAP_PATH folder, or %s could not be copied\n", hangFrom);

    t = nowMs();
    mdaw_engine_destroy (e);
    bench ("engine_destroy_ms", nowMs() - t, "ms");
    bench ("total_ms", nowMs() - started, "ms");
    return 0;
}
