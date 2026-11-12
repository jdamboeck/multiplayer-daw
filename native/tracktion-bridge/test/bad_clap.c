/* CLAP plugins for the bridge test, one per variant, all from this file (CMakeLists.txt):
 *   MDAW_BAD_CRASH      clap_entry.init calls abort()          bad-crash.clap
 *   MDAW_BAD_HANG       clap_entry.init never returns           bad-hang.clap
 *   MDAW_BAD_STATELESS  a well-behaved plugin with one parameter and no state extension; it
 *                       outputs silence                         test-stateless.clap
 * The first two are what the plugin scanner has to survive; the third is what
 * mdaw_device_reset_state does with a plugin that saves no state. */
#define _POSIX_C_SOURCE 200809L
#include <clap/clap.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if defined(MDAW_BAD_CRASH)
#define PLUGIN_ID "org.mdaw.test-bad-crash"
#elif defined(MDAW_BAD_HANG)
#define PLUGIN_ID "org.mdaw.test-bad-hang"
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
    return NULL;   /* no CLAP_EXT_STATE: the point of this plugin */
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
