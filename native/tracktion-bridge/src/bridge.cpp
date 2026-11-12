// The C functions of mdaw_engine.h: argument checks, marshalling, and the boundary that no
// exception crosses. Each call is carried out on the engine's message thread (Project).
#include "mdaw_engine.h"
#include "engine_thread.h"
#include "project.h"

#include <cstring>
#include <mutex>
#include <string>

using mdaw::bridge::Failure;
using mdaw::bridge::Project;

struct mdaw_engine
{
    mdaw::bridge::Shared shared;
    mdaw::bridge::EngineThread thread;
    std::unique_ptr<Project> project;   // created and deleted on the message thread
};

namespace
{
thread_local std::string lastError;
thread_local mdaw_status lastStatus = MDAW_OK;

std::mutex lifeMutex;
mdaw_engine* liveEngine = nullptr;   // one engine per process

void setError (mdaw_status status, std::string message)
{
    lastStatus = status;
    lastError = std::move (message);
}

/// Runs `fn` and turns whatever it throws into a status and the calling thread's error text.
template <typename Fn>
mdaw_status guarded (Fn&& fn) noexcept
{
    try
    {
        fn();
        return MDAW_OK;
    }
    catch (const Failure& f)            { setError (f.status, f.what()); return f.status; }
    catch (const std::exception& e)     { setError (MDAW_ERR_INTERNAL, e.what()); return MDAW_ERR_INTERNAL; }
    catch (...)                         { setError (MDAW_ERR_INTERNAL, "unknown C++ exception"); return MDAW_ERR_INTERNAL; }
}

[[noreturn]] void argumentError (const char* what) { throw Failure (MDAW_ERR_ARGUMENT, what); }

/// Runs `fn` with the engine's project on the message thread and waits for it.
template <typename Fn>
mdaw_status onEngine (mdaw_engine* engine, Fn&& fn) noexcept
{
    return guarded ([&]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        engine->thread.call ([&] { fn (*engine->project); });
    });
}

juce::String text (const char* s) { return s != nullptr ? juce::String::fromUTF8 (s) : juce::String(); }

/// Copies text and its NUL if both fit; returns the length without the NUL.
size_t copyOut (const juce::String& s, char* buffer, size_t capacity)
{
    const auto* utf8 = s.toRawUTF8();
    const auto length = std::strlen (utf8);
    if (buffer != nullptr && capacity > length)
        std::memcpy (buffer, utf8, length + 1);
    else if (buffer != nullptr && capacity > 0)
        buffer[0] = 0;
    return length;
}

/// For the functions that return a length: 0 on failure, with the error text set.
template <typename Fn>
size_t textFrom (mdaw_engine* engine, char* buffer, size_t capacity, Fn&& fn) noexcept
{
    juce::String result;
    if (onEngine (engine, [&] (Project& p) { result = fn (p); }) != MDAW_OK)
        return 0;
    try
    {
        return copyOut (result, buffer, capacity);
    }
    catch (...)
    {
        setError (MDAW_ERR_INTERNAL, "could not copy the result");
        return 0;
    }
}

juce::File fileOf (const char* path)
{
    return juce::File::getCurrentWorkingDirectory().getChildFile (text (path));
}
} // namespace

extern "C" {

mdaw_engine* mdaw_engine_create (const mdaw_engine_config* config)
{
    try
    {
        if (config == nullptr) { setError (MDAW_ERR_ARGUMENT, "config is NULL"); return nullptr; }
        if (config->abi_version != MDAW_ABI_VERSION) { setError (MDAW_ERR_ARGUMENT, "abi_version " + std::to_string (config->abi_version) + " is not " + std::to_string (MDAW_ABI_VERSION)); return nullptr; }
        if (config->settings_dir == nullptr || *config->settings_dir == 0) { setError (MDAW_ERR_ARGUMENT, "settings_dir is empty"); return nullptr; }

        const std::scoped_lock lock (lifeMutex);
        // Not a bad argument and nothing missing: "anything else".
        if (liveEngine != nullptr) { setError (MDAW_ERR_INTERNAL, "an engine already exists in this process"); return nullptr; }

        mdaw::bridge::Config c;
        c.appName = config->app_name != nullptr && *config->app_name != 0 ? text (config->app_name) : juce::String ("mdaw");
        c.settingsDir = fileOf (config->settings_dir);
        c.openDevice = config->open_audio_device != 0;
        c.backend = text (config->audio_backend);
        c.enablePlugins = config->enable_plugins != 0;

        auto engine = std::make_unique<mdaw_engine>();
        auto* e = engine.get();
        e->thread.start ([e, c] { e->project = std::make_unique<Project> (c, e->shared); });
        liveEngine = engine.release();
        return liveEngine;
    }
    catch (const Failure& f) { setError (f.status, f.what()); }
    catch (const std::exception& e) { setError (MDAW_ERR_INTERNAL, e.what()); }
    catch (...) { setError (MDAW_ERR_INTERNAL, "unknown C++ exception"); }
    return nullptr;
}

void mdaw_engine_destroy (mdaw_engine* engine)
{
    if (engine == nullptr)
        return;
    guarded ([engine]
    {
        Project::Steps steps;
        if (engine->project != nullptr)
            steps = engine->project->shutDownSteps();
        engine->thread.stop (std::move (steps), Project::teardownGapMs, [engine] { engine->project.reset(); });
    });
    const std::scoped_lock lock (lifeMutex);
    if (liveEngine == engine)
        liveEngine = nullptr;
    delete engine;
}

uint32_t mdaw_abi_version (void) { return MDAW_ABI_VERSION; }

size_t mdaw_last_error (char* buffer, size_t capacity)
{
    const auto length = lastError.size();
    if (buffer != nullptr && capacity > length)
        std::memcpy (buffer, lastError.c_str(), length + 1);
    else if (buffer != nullptr && capacity > 0)
        buffer[0] = 0;
    return length;
}

mdaw_status mdaw_last_status (void) { return lastStatus; }

size_t mdaw_engine_versions (char* buffer, size_t capacity)
{
    try
    {
        // Both already say what they are: "Tracktion Engine v3.1.0", "JUCE v8.0.6".
        return copyOut (tracktion::Engine::getVersion() + " / " + juce::SystemStats::getJUCEVersion(), buffer, capacity);
    }
    catch (...)
    {
        setError (MDAW_ERR_INTERNAL, "could not build the version text");
        return 0;
    }
}

//==============================================================================
int32_t mdaw_project_has (mdaw_engine* engine, int32_t kind, uint64_t id)
{
    bool result = false;
    if (onEngine (engine, [&] (Project& p) { result = p.has (kind, id); }) != MDAW_OK)
        return 0;
    return result ? 1 : 0;
}

mdaw_status mdaw_project_clear (mdaw_engine* engine)
{
    return guarded ([engine]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        // Making the steps touches nothing; running them happens on the message thread.
        engine->thread.call (engine->project->clearSteps(), Project::teardownGapMs);
    });
}

mdaw_status mdaw_tempo_put (mdaw_engine* engine, uint64_t tempo_id, int64_t tick, uint32_t milli_bpm)
{
    return onEngine (engine, [&] (Project& p) { p.tempoPut (tempo_id, tick, milli_bpm); });
}

mdaw_status mdaw_tempo_remove (mdaw_engine* engine, uint64_t tempo_id)
{
    return onEngine (engine, [&] (Project& p) { p.tempoRemove (tempo_id); });
}

mdaw_status mdaw_master_set_gain (mdaw_engine* engine, float gain)
{
    return onEngine (engine, [&] (Project& p) { p.masterSetGain (gain); });
}

mdaw_status mdaw_track_add (mdaw_engine* engine, uint64_t track, uint64_t after_track, const char* name)
{
    return onEngine (engine, [&] (Project& p) { p.trackAdd (track, after_track, text (name)); });
}

mdaw_status mdaw_track_remove (mdaw_engine* engine, uint64_t track)
{
    return onEngine (engine, [&] (Project& p) { p.trackRemove (track); });
}

mdaw_status mdaw_track_move (mdaw_engine* engine, uint64_t track, uint64_t after_track)
{
    return onEngine (engine, [&] (Project& p) { p.trackMove (track, after_track); });
}

mdaw_status mdaw_track_set_name (mdaw_engine* engine, uint64_t track, const char* name)
{
    return onEngine (engine, [&] (Project& p) { p.trackSetName (track, text (name)); });
}

mdaw_status mdaw_track_set_mix (mdaw_engine* engine, uint64_t track, float gain, float pan, int32_t mute)
{
    return onEngine (engine, [&] (Project& p) { p.trackSetMix (track, gain, pan, mute != 0); });
}

mdaw_status mdaw_clip_add (mdaw_engine* engine, uint64_t track, uint64_t clip, int64_t start_tick, int64_t length_ticks, const char* name)
{
    return onEngine (engine, [&] (Project& p) { p.clipAdd (track, clip, start_tick, length_ticks, text (name)); });
}

mdaw_status mdaw_clip_remove (mdaw_engine* engine, uint64_t clip)
{
    return onEngine (engine, [&] (Project& p) { p.clipRemove (clip); });
}

mdaw_status mdaw_clip_set_range (mdaw_engine* engine, uint64_t clip, int64_t start_tick, int64_t length_ticks)
{
    return onEngine (engine, [&] (Project& p) { p.clipSetRange (clip, start_tick, length_ticks); });
}

mdaw_status mdaw_clip_set_name (mdaw_engine* engine, uint64_t clip, const char* name)
{
    return onEngine (engine, [&] (Project& p) { p.clipSetName (clip, text (name)); });
}

mdaw_status mdaw_notes_put (mdaw_engine* engine, uint64_t clip, const mdaw_note* notes, size_t count)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (notes == nullptr && count > 0) argumentError ("notes is NULL");
        p.notesPut (clip, notes, count);
    });
}

mdaw_status mdaw_notes_remove (mdaw_engine* engine, uint64_t clip, const uint64_t* note_ids, size_t count)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (note_ids == nullptr && count > 0) argumentError ("note_ids is NULL");
        p.notesRemove (clip, note_ids, count);
    });
}

mdaw_status mdaw_device_add (mdaw_engine* engine, uint64_t track, uint64_t device, uint64_t after_device,
                             const char* format, const char* identifier)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (format == nullptr || identifier == nullptr) argumentError ("format or identifier is NULL");
        p.deviceAdd (track, device, after_device, text (format), text (identifier));
    });
}

mdaw_status mdaw_device_remove (mdaw_engine* engine, uint64_t device)
{
    return onEngine (engine, [&] (Project& p) { p.deviceRemove (device); });
}

mdaw_status mdaw_device_move (mdaw_engine* engine, uint64_t device, uint64_t after_device)
{
    return onEngine (engine, [&] (Project& p) { p.deviceMove (device, after_device); });
}

mdaw_status mdaw_device_set_bypass (mdaw_engine* engine, uint64_t device, int32_t bypass)
{
    return onEngine (engine, [&] (Project& p) { p.deviceSetBypass (device, bypass != 0); });
}

mdaw_status mdaw_device_set_param (mdaw_engine* engine, uint64_t device, const char* param_id, float value)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (param_id == nullptr) argumentError ("param_id is NULL");
        p.deviceSetParam (device, text (param_id), value);
    });
}

mdaw_status mdaw_device_get_param (mdaw_engine* engine, uint64_t device, const char* param_id, float* value)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (param_id == nullptr || value == nullptr) argumentError ("param_id or value is NULL");
        *value = p.deviceGetParam (device, text (param_id));
    });
}

size_t mdaw_device_params (mdaw_engine* engine, uint64_t device, char* buffer, size_t capacity)
{
    return textFrom (engine, buffer, capacity, [&] (Project& p) { return p.deviceParams (device); });
}

size_t mdaw_device_param_text (mdaw_engine* engine, uint64_t device, const char* param_id, float value,
                               char* buffer, size_t capacity)
{
    return textFrom (engine, buffer, capacity, [&] (Project& p)
    {
        if (param_id == nullptr) argumentError ("param_id is NULL");
        return p.deviceParamText (device, text (param_id), value);
    });
}

size_t mdaw_device_get_state (mdaw_engine* engine, uint64_t device, uint8_t* buffer, size_t capacity)
{
    juce::MemoryBlock state;
    if (onEngine (engine, [&] (Project& p) { state = p.deviceGetState (device); }) != MDAW_OK)
        return 0;
    if (buffer != nullptr && capacity >= state.getSize())
        std::memcpy (buffer, state.getData(), state.getSize());
    return state.getSize();
}

mdaw_status mdaw_device_set_state (mdaw_engine* engine, uint64_t device, const uint8_t* bytes, size_t length)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (bytes == nullptr || length == 0) argumentError ("no state given");
        p.deviceSetState (device, bytes, length);
    });
}

mdaw_status mdaw_device_reset_state (mdaw_engine* engine, uint64_t device)
{
    return onEngine (engine, [&] (Project& p) { p.deviceResetState (device); });
}

mdaw_status mdaw_device_show_editor (mdaw_engine* engine, uint64_t device, int32_t show)
{
    return onEngine (engine, [&] (Project& p) { p.deviceShowEditor (device, show != 0); });
}

//==============================================================================
mdaw_status mdaw_plugins_scan (mdaw_engine* engine, const char* format)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (format == nullptr) argumentError ("format is NULL");
        p.pluginsScan (text (format));
    });
}

size_t mdaw_plugins_list (mdaw_engine* engine, char* buffer, size_t capacity)
{
    return textFrom (engine, buffer, capacity, [] (Project& p) { return p.pluginsList(); });
}

//==============================================================================
mdaw_status mdaw_transport_play (mdaw_engine* engine)
{
    return onEngine (engine, [] (Project& p) { p.play(); });
}

mdaw_status mdaw_transport_stop (mdaw_engine* engine)
{
    return onEngine (engine, [] (Project& p) { p.stop(); });
}

mdaw_status mdaw_transport_seek (mdaw_engine* engine, int64_t tick)
{
    return onEngine (engine, [&] (Project& p) { p.seek (tick); });
}

mdaw_status mdaw_transport_set_loop (mdaw_engine* engine, int64_t start_tick, int64_t end_tick)
{
    return onEngine (engine, [&] (Project& p) { p.setLoop (start_tick, end_tick); });
}

void mdaw_engine_state_read (mdaw_engine* engine, mdaw_engine_state* out)
{
    if (out == nullptr)
        return;
    *out = {};
    if (engine == nullptr)
        return;
    auto& s = engine->shared;
    out->playing = s.playing.load();
    out->position_tick = s.positionTick.load();
    out->position_seconds = s.positionSeconds.load();
    out->device_open = s.deviceOpen.load();
    out->sample_rate = s.sampleRate.load();
    out->block_size = s.blockSize.load();
    out->xruns = s.xruns.load();
    out->cpu = s.cpu.load();
    out->peak_left = s.peakLeft.exchange (0.0f);
    out->peak_right = s.peakRight.exchange (0.0f);
}

size_t mdaw_audio_devices (mdaw_engine* engine, char* buffer, size_t capacity)
{
    return textFrom (engine, buffer, capacity, [] (Project& p) { return p.audioDevices(); });
}

mdaw_status mdaw_audio_device_set (mdaw_engine* engine, const char* backend, const char* output_device,
                                   double sample_rate, int32_t block_size)
{
    return onEngine (engine, [&] (Project& p) { p.audioDeviceSet (text (backend), text (output_device), sample_rate, block_size); });
}

mdaw_status mdaw_render (mdaw_engine* engine, const char* wav_path, int64_t start_tick, int64_t end_tick, double sample_rate)
{
    return onEngine (engine, [&] (Project& p)
    {
        if (wav_path == nullptr || *wav_path == 0) argumentError ("wav_path is empty");
        p.render (fileOf (wav_path), start_tick, end_tick, sample_rate);
    });
}

//==============================================================================
int32_t mdaw_engine_poll_event (mdaw_engine* engine, mdaw_event* out)
{
    if (engine == nullptr || out == nullptr)
    {
        setError (MDAW_ERR_ARGUMENT, "engine or out is NULL");
        return 0;
    }
    try
    {
        return engine->shared.pop (*out) ? 1 : 0;
    }
    catch (...)
    {
        setError (MDAW_ERR_INTERNAL, "could not read the event queue");
        return 0;
    }
}

} // extern "C"
