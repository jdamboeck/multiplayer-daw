/* CLAP plugins for the bridge test, one per variant, all from this file (CMakeLists.txt):
 *   MDAW_BAD_CRASH      clap_entry.init calls abort()          bad-crash.clap
 *   MDAW_BAD_HANG       clap_entry.init never returns           bad-hang.clap
 *   MDAW_BAD_STATELESS  a well-behaved plugin with one parameter and no state extension; it
 *                       outputs silence                         test-stateless.clap
 *   MDAW_TEST_GUI       as the stateless one, plus a state extension (the level as 8 raw bytes)
 *                       and an X11 window extension that makes no window of its own
 *                                                               test-gui.clap
 * The first two are what the plugin scanner has to survive; the third is what
 * mdaw_device_reset_state does with a plugin that saves no state; the fourth is for the editor
 * benchmarks (test/editor_bench.c, crates/mdaw-client/tests/editor_latency.rs). Its slowness is
 * set from the environment, read at each call:
 *   MDAW_TEST_STATE_DELAY_MS  milliseconds state.save sleeps before writing
 *   MDAW_TEST_GUI_DELAY_MS    milliseconds gui.create sleeps
 *   MDAW_TEST_GUI_FAIL        non-empty: gui.create fails */
#define _POSIX_C_SOURCE 200809L
#include <clap/clap.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if defined(MDAW_BAD_CRASH)
#define PLUGIN_ID "org.mdaw.test-bad-crash"
#elif defined(MDAW_BAD_HANG)
#define PLUGIN_ID "org.mdaw.test-bad-hang"
#elif defined(MDAW_TEST_GUI)
#define PLUGIN_ID "org.mdaw.test-gui"
#else
#define PLUGIN_ID "org.mdaw.test-stateless"
#endif

static const char* const features[] = { CLAP_PLUGIN_FEATURE_INSTRUMENT, NULL };
static const clap_plugin_descriptor_t descriptor = {
    CLAP_VERSION_INIT, PLUGIN_ID, "mdaw " PLUGIN_ID, "mdaw", "", "", "", "1.0", "", features
};

static double level = 0.5;   /* the one parameter, id 0 */

static void applyEvents (const clap_input_events_t* in)
{
    for (uint32_t i = 0; in != NULL && i < in->size (in); ++i)
    {
        const clap_event_header_t* h = in->get (in, i);
        if (h->space_id == CLAP_CORE_EVENT_SPACE_ID && h->type == CLAP_EVENT_PARAM_VALUE)
            level = ((const clap_event_param_value_t*) h)->value;
    }
}

static uint32_t paramCount (const clap_plugin_t* p) { (void) p; return 1; }
static bool paramInfo (const clap_plugin_t* p, uint32_t index, clap_param_info_t* info)
{
    (void) p;
    if (index != 0) return false;
    memset (info, 0, sizeof *info);
    info->flags = CLAP_PARAM_IS_AUTOMATABLE;
    strcpy (info->name, "Level");
    info->max_value = 1.0;
    info->default_value = 0.5;
    return true;
}
static bool paramValue (const clap_plugin_t* p, clap_id id, double* out) { (void) p; *out = level; return id == 0; }
static bool paramText (const clap_plugin_t* p, clap_id id, double v, char* out, uint32_t n) { (void) p; snprintf (out, n, "%.2f", v); return id == 0; }
static bool paramParse (const clap_plugin_t* p, clap_id id, const char* t, double* out) { (void) p; *out = atof (t); return id == 0; }
static void paramFlush (const clap_plugin_t* p, const clap_input_events_t* in, const clap_output_events_t* o) { (void) p; (void) o; applyEvents (in); }
static const clap_plugin_params_t params = { paramCount, paramInfo, paramValue, paramText, paramParse, paramFlush };

static uint32_t portCount (const clap_plugin_t* p, bool input) { (void) p; return input ? 0 : 1; }
static bool portInfo (const clap_plugin_t* p, uint32_t index, bool input, clap_audio_port_info_t* info)
{
    (void) p;
    if (input || index != 0) return false;
    memset (info, 0, sizeof *info);
    strcpy (info->name, "Out");
    info->flags = CLAP_AUDIO_PORT_IS_MAIN;
    info->channel_count = 2;
    info->port_type = CLAP_PORT_STEREO;
    info->in_place_pair = CLAP_INVALID_ID;
    return true;
}
static const clap_plugin_audio_ports_t ports = { portCount, portInfo };

#if defined(MDAW_TEST_GUI)
/* Milliseconds from an environment variable read now; 0 if unset or not a positive integer. */
static long envMs (const char* name)
{
    const char* text = getenv (name);
    if (text == NULL || *text == 0) return 0;
    char* end = NULL;
    long value = strtol (text, &end, 10);
    return (end != NULL && *end == 0 && value > 0) ? value : 0;
}

static void sleepMs (long ms)
{
    if (ms <= 0) return;
    struct timespec t = { ms / 1000, (ms % 1000) * 1000000L };
    while (nanosleep (&t, &t) != 0 && errno == EINTR) {}
}

static bool stateSave (const clap_plugin_t* p, const clap_ostream_t* stream)
{
    (void) p;
    sleepMs (envMs ("MDAW_TEST_STATE_DELAY_MS"));
    const unsigned char* bytes = (const unsigned char*) &level;
    uint64_t done = 0;
    while (done < sizeof level)
    {
        int64_t n = stream->write (stream, bytes + done, sizeof level - done);
        if (n <= 0) return false;
        done += (uint64_t) n;
    }
    return true;
}
static bool stateLoad (const clap_plugin_t* p, const clap_istream_t* stream)
{
    (void) p;
    double value = 0.0;
    unsigned char* bytes = (unsigned char*) &value;
    uint64_t done = 0;
    while (done < sizeof value)
    {
        int64_t n = stream->read (stream, bytes + done, sizeof value - done);
        if (n <= 0) return false;
        done += (uint64_t) n;
    }
    level = value;
    return true;
}
static const clap_plugin_state_t state = { stateSave, stateLoad };

/* Counted for debugging only; nothing reads them. */
static int guiCreated = 0, guiDestroyed = 0;

static bool guiApiSupported (const clap_plugin_t* p, const char* api, bool floating) { (void) p; return ! floating && strcmp (api, CLAP_WINDOW_API_X11) == 0; }
static bool guiPreferredApi (const clap_plugin_t* p, const char** api, bool* floating) { (void) p; *api = CLAP_WINDOW_API_X11; *floating = false; return true; }
static bool guiCreate (const clap_plugin_t* p, const char* api, bool floating)
{
    (void) p; (void) api; (void) floating;
    sleepMs (envMs ("MDAW_TEST_GUI_DELAY_MS"));
    const char* fail = getenv ("MDAW_TEST_GUI_FAIL");
    if (fail != NULL && *fail != 0) return false;
    ++guiCreated;
    return true;
}
static void guiDestroy (const clap_plugin_t* p) { (void) p; ++guiDestroyed; }
static bool guiSetScale (const clap_plugin_t* p, double scale) { (void) p; (void) scale; return false; }
static bool guiGetSize (const clap_plugin_t* p, uint32_t* w, uint32_t* h) { (void) p; *w = 320; *h = 200; return true; }
static bool guiCanResize (const clap_plugin_t* p) { (void) p; return false; }
static bool guiResizeHints (const clap_plugin_t* p, clap_gui_resize_hints_t* hints) { (void) p; (void) hints; return false; }
static bool guiAdjustSize (const clap_plugin_t* p, uint32_t* w, uint32_t* h) { (void) p; (void) w; (void) h; return false; }
static bool guiSetSize (const clap_plugin_t* p, uint32_t w, uint32_t h) { (void) p; (void) w; (void) h; return false; }
/* No window of its own: the host's embedding shows its own black area. */
static bool guiSetParent (const clap_plugin_t* p, const clap_window_t* window) { (void) p; (void) window; return true; }
static bool guiSetTransient (const clap_plugin_t* p, const clap_window_t* window) { (void) p; (void) window; return true; }
static void guiSuggestTitle (const clap_plugin_t* p, const char* title) { (void) p; (void) title; }
static bool guiShow (const clap_plugin_t* p) { (void) p; return true; }
static bool guiHide (const clap_plugin_t* p) { (void) p; return true; }
static const clap_plugin_gui_t gui = { guiApiSupported, guiPreferredApi, guiCreate, guiDestroy, guiSetScale, guiGetSize,
                                       guiCanResize, guiResizeHints, guiAdjustSize, guiSetSize, guiSetParent,
                                       guiSetTransient, guiSuggestTitle, guiShow, guiHide };
#endif

static bool pluginInit (const clap_plugin_t* p) { (void) p; return true; }
static void pluginDestroy (const clap_plugin_t* p) { free ((void*) p); }
static bool pluginActivate (const clap_plugin_t* p, double r, uint32_t a, uint32_t b) { (void) p; (void) r; (void) a; (void) b; return true; }
static void pluginNothing (const clap_plugin_t* p) { (void) p; }
static bool pluginStart (const clap_plugin_t* p) { (void) p; return true; }
static clap_process_status pluginProcess (const clap_plugin_t* p, const clap_process_t* process)
{
    (void) p;
    applyEvents (process->in_events);
    for (uint32_t c = 0; process->audio_outputs_count > 0 && c < process->audio_outputs[0].channel_count; ++c)
        if (process->audio_outputs[0].data32 != NULL)
            memset (process->audio_outputs[0].data32[c], 0, sizeof (float) * process->frames_count);
    return CLAP_PROCESS_CONTINUE;
}
static const void* pluginExtension (const clap_plugin_t* p, const char* id)
{
    (void) p;
    if (strcmp (id, CLAP_EXT_PARAMS) == 0) return &params;
    if (strcmp (id, CLAP_EXT_AUDIO_PORTS) == 0) return &ports;
#if defined(MDAW_TEST_GUI)
    if (strcmp (id, CLAP_EXT_STATE) == 0) return &state;
    if (strcmp (id, CLAP_EXT_GUI) == 0) return &gui;
#endif
    return NULL;   /* the stateless plugin: no CLAP_EXT_STATE, the point of it */
}

static uint32_t factoryCount (const clap_plugin_factory_t* f) { (void) f; return 1; }
static const clap_plugin_descriptor_t* factoryDescriptor (const clap_plugin_factory_t* f, uint32_t i) { (void) f; return i == 0 ? &descriptor : NULL; }
static const clap_plugin_t* factoryCreate (const clap_plugin_factory_t* f, const clap_host_t* host, const char* id)
{
    (void) f;
    if (strcmp (id, PLUGIN_ID) != 0) return NULL;
    clap_plugin_t* p = calloc (1, sizeof *p);
    *p = (clap_plugin_t) { &descriptor, NULL, pluginInit, pluginDestroy, pluginActivate, pluginNothing, pluginStart,
                           pluginNothing, pluginNothing, pluginProcess, pluginExtension, pluginNothing };
    (void) host;
    return p;
}
static const clap_plugin_factory_t factory = { factoryCount, factoryDescriptor, factoryCreate };

static bool entryInit (const char* path)
{
    (void) path;
#if defined(MDAW_BAD_CRASH)
    abort();
#elif defined(MDAW_BAD_HANG)
    for (;;)
    {
        struct timespec t = { 3600, 0 };
        nanosleep (&t, NULL);
    }
#endif
    return true;
}
static void entryDeinit (void) {}
static const void* entryFactory (const char* id) { return strcmp (id, CLAP_PLUGIN_FACTORY_ID) == 0 ? &factory : NULL; }

CLAP_EXPORT const clap_plugin_entry_t clap_entry = { CLAP_VERSION_INIT, entryInit, entryDeinit, entryFactory };
