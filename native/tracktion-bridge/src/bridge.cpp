// The C functions of mdaw_engine.h: argument checks, marshalling, and the boundary that no
// exception crosses. Each call is carried out on the engine's message thread (Project).
#include "mdaw_engine.h"
#include "mdaw_engine_test.h"
#include "engine_thread.h"
#include "plugin_scan.h"
#include "project.h"
#include "trace.h"

#include <algorithm>
#include <atomic>
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
    std::mutex scanMutex;                // one plugin scan at a time; held by the scanning caller
    std::atomic<bool> scanCancelled { false };   // mdaw_plugins_scan_cancel; never cleared
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

/// A state mdaw_device_get_state read that did not fit the caller's buffer, kept for the same
/// thread's next call (see the header).
struct KeptState
{
    mdaw_engine* engine = nullptr;
    uint64_t device = 0;
    juce::MemoryBlock block;
    bool full = false;

    void clear() { *this = {}; }
};
thread_local KeptState keptState;

/// Runs `fn` and turns whatever it throws into a status and the calling thread's error text.
template <typename Fn>
mdaw_status guarded (Fn&& fn) noexcept
{
    try
    {
        // Any call drops a state kept for the next mdaw_device_get_state.
        if (keptState.full)
            keptState.clear();
        fn();
        return MDAW_OK;
    }
    catch (const Failure& f)            { setError (f.status, f.what()); return f.status; }
    catch (const std::exception& e)     { setError (MDAW_ERR_INTERNAL, e.what()); return MDAW_ERR_INTERNAL; }
    catch (...)                         { setError (MDAW_ERR_INTERNAL, "unknown C++ exception"); return MDAW_ERR_INTERNAL; }
}

[[noreturn]] void argumentError (const char* what) { throw Failure (MDAW_ERR_ARGUMENT, what); }

/// Runs `fn` with the engine's project on the message thread and waits for it. `label` names
/// the call in traces and the log (EngineThread::call).
template <typename Fn>
mdaw_status onEngine (mdaw_engine* engine, const char* label, Fn&& fn) noexcept
{
    return guarded ([&]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        engine->thread.call ([&] { fn (*engine->project); }, label);
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
size_t textFrom (mdaw_engine* engine, const char* label, char* buffer, size_t capacity, Fn&& fn) noexcept
{
    juce::String result;
    if (onEngine (engine, label, [&] (Project& p) { result = fn (p); }) != MDAW_OK)
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
        if (config->scanner_path != nullptr && *config->scanner_path != 0)
            c.scanner = fileOf (config->scanner_path);

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
    if (onEngine (engine, "project has", [&] (Project& p) { result = p.has (kind, id); }) != MDAW_OK)
        return 0;
    return result ? 1 : 0;
}

mdaw_status mdaw_project_clear (mdaw_engine* engine)
{
    return guarded ([engine]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        // An edit nothing has changed is kept: no teardown, no gaps. Decided on the message
        // thread; whatever comes in after this call is wiped by the full clear below anyway.
        bool done = false;
        engine->thread.call ([&] { done = engine->project->clearIfUnchanged(); }, "project clear");
        if (done)
            return;
        // Making the steps touches nothing; running them happens on the message thread.
        engine->thread.call (engine->project->clearSteps(), Project::teardownGapMs, "project clear (new edit)");
    });
}

mdaw_status mdaw_tempo_put (mdaw_engine* engine, uint64_t tempo_id, int64_t tick, uint32_t milli_bpm)
{
    return onEngine (engine, "tempo put", [&] (Project& p) { p.tempoPut (tempo_id, tick, milli_bpm); });
}

mdaw_status mdaw_tempo_remove (mdaw_engine* engine, uint64_t tempo_id)
{
    return onEngine (engine, "tempo remove", [&] (Project& p) { p.tempoRemove (tempo_id); });
}

mdaw_status mdaw_master_set_gain (mdaw_engine* engine, float gain)
{
    return onEngine (engine, "master set gain", [&] (Project& p) { p.masterSetGain (gain); });
}

mdaw_status mdaw_track_add (mdaw_engine* engine, uint64_t track, uint64_t after_track, const char* name)
{
    return onEngine (engine, "track add", [&] (Project& p) { p.trackAdd (track, after_track, text (name)); });
}

mdaw_status mdaw_track_remove (mdaw_engine* engine, uint64_t track)
{
    return onEngine (engine, "track remove", [&] (Project& p) { p.trackRemove (track); });
}

mdaw_status mdaw_track_move (mdaw_engine* engine, uint64_t track, uint64_t after_track)
{
    return onEngine (engine, "track move", [&] (Project& p) { p.trackMove (track, after_track); });
}

mdaw_status mdaw_track_set_name (mdaw_engine* engine, uint64_t track, const char* name)
{
    return onEngine (engine, "track set name", [&] (Project& p) { p.trackSetName (track, text (name)); });
}

mdaw_status mdaw_track_set_mix (mdaw_engine* engine, uint64_t track, float gain, float pan, int32_t mute)
{
    return onEngine (engine, "track set mix", [&] (Project& p) { p.trackSetMix (track, gain, pan, mute != 0); });
}

mdaw_status mdaw_track_live_note (mdaw_engine* engine, uint64_t track, uint8_t pitch, uint8_t velocity)
{
    return guarded ([&]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        if (pitch > 127 || velocity > 127) argumentError ("pitch and velocity must be 0..127");
        // Not through call(): a note must not wait for, nor make its caller wait for, a call in
        // progress on another thread. The project outlives every message posted while it lives
        // (it is deleted after the dispatch loop has ended).
        if (! engine->thread.post ([engine, track, pitch, velocity]
                                   {
                                       if (engine->project != nullptr)
                                           engine->project->trackLiveNote (track, pitch, velocity);
                                   }))
            throw Failure (MDAW_ERR_INTERNAL, "the engine's message thread is not running");
    });
}

mdaw_status mdaw_clip_add (mdaw_engine* engine, uint64_t track, uint64_t clip, int64_t start_tick, int64_t length_ticks, const char* name)
{
    return onEngine (engine, "clip add", [&] (Project& p) { p.clipAdd (track, clip, start_tick, length_ticks, text (name)); });
}

mdaw_status mdaw_clip_remove (mdaw_engine* engine, uint64_t clip)
{
    return onEngine (engine, "clip remove", [&] (Project& p) { p.clipRemove (clip); });
}

mdaw_status mdaw_clip_set_range (mdaw_engine* engine, uint64_t clip, int64_t start_tick, int64_t length_ticks)
{
    return onEngine (engine, "clip set range", [&] (Project& p) { p.clipSetRange (clip, start_tick, length_ticks); });
}

mdaw_status mdaw_clip_set_name (mdaw_engine* engine, uint64_t clip, const char* name)
{
    return onEngine (engine, "clip set name", [&] (Project& p) { p.clipSetName (clip, text (name)); });
}

mdaw_status mdaw_notes_put (mdaw_engine* engine, uint64_t clip, const mdaw_note* notes, size_t count)
{
    return onEngine (engine, "notes put", [&] (Project& p)
    {
        if (notes == nullptr && count > 0) argumentError ("notes is NULL");
        p.notesPut (clip, notes, count);
    });
}

mdaw_status mdaw_notes_remove (mdaw_engine* engine, uint64_t clip, const uint64_t* note_ids, size_t count)
{
    return onEngine (engine, "notes remove", [&] (Project& p)
    {
        if (note_ids == nullptr && count > 0) argumentError ("note_ids is NULL");
        p.notesRemove (clip, note_ids, count);
    });
}

mdaw_status mdaw_device_add (mdaw_engine* engine, uint64_t track, uint64_t device, uint64_t after_device,
                             const char* format, const char* identifier)
{
    return onEngine (engine, "device add", [&] (Project& p)
    {
        if (format == nullptr || identifier == nullptr) argumentError ("format or identifier is NULL");
        p.deviceAdd (track, device, after_device, text (format), text (identifier));
    });
}

mdaw_status mdaw_device_remove (mdaw_engine* engine, uint64_t device)
{
    return onEngine (engine, "device remove", [&] (Project& p) { p.deviceRemove (device); });
}

mdaw_status mdaw_device_move (mdaw_engine* engine, uint64_t device, uint64_t after_device)
{
    return onEngine (engine, "device move", [&] (Project& p) { p.deviceMove (device, after_device); });
}

mdaw_status mdaw_device_set_bypass (mdaw_engine* engine, uint64_t device, int32_t bypass)
{
    return onEngine (engine, "device set bypass", [&] (Project& p) { p.deviceSetBypass (device, bypass != 0); });
}

mdaw_status mdaw_device_set_param (mdaw_engine* engine, uint64_t device, const char* param_id, float value)
{
    return onEngine (engine, "device set param", [&] (Project& p)
    {
        if (param_id == nullptr) argumentError ("param_id is NULL");
        p.deviceSetParam (device, text (param_id), value);
    });
}

mdaw_status mdaw_device_get_param (mdaw_engine* engine, uint64_t device, const char* param_id, float* value)
{
    return onEngine (engine, "device get param", [&] (Project& p)
    {
        if (param_id == nullptr || value == nullptr) argumentError ("param_id or value is NULL");
        *value = p.deviceGetParam (device, text (param_id));
    });
}

size_t mdaw_device_params (mdaw_engine* engine, uint64_t device, char* buffer, size_t capacity)
{
    return textFrom (engine, "device params", buffer, capacity, [&] (Project& p) { return p.deviceParams (device); });
}

size_t mdaw_device_param_text (mdaw_engine* engine, uint64_t device, const char* param_id, float value,
                               char* buffer, size_t capacity)
{
    return textFrom (engine, "device param text", buffer, capacity, [&] (Project& p)
    {
        if (param_id == nullptr) argumentError ("param_id is NULL");
        return p.deviceParamText (device, text (param_id), value);
    });
}

size_t mdaw_device_get_state (mdaw_engine* engine, uint64_t device, uint8_t* buffer, size_t capacity)
{
    // The state the previous call of this thread read and could not hand over, if it was for
    // this device: it is taken now, without asking the plugin again.
    KeptState kept;
    std::swap (kept, keptState);
    if (kept.full && kept.engine == engine && kept.device == device && buffer != nullptr && capacity >= kept.block.getSize())
    {
        std::memcpy (buffer, kept.block.getData(), kept.block.getSize());
        return kept.block.getSize();
    }

    juce::MemoryBlock state;
    if (onEngine (engine, "device get state", [&] (Project& p) { state = p.deviceGetState (device); }) != MDAW_OK)
        return 0;
    if (buffer != nullptr && capacity >= state.getSize())
    {
        std::memcpy (buffer, state.getData(), state.getSize());
    }
    else
    {
        keptState.engine = engine;
        keptState.device = device;
        keptState.block = std::move (state);
        keptState.full = true;
        return keptState.block.getSize();
    }
    return state.getSize();
}

mdaw_status mdaw_device_set_state (mdaw_engine* engine, uint64_t device, const uint8_t* bytes, size_t length)
{
    return onEngine (engine, "device set state", [&] (Project& p)
    {
        if (bytes == nullptr || length == 0) argumentError ("no state given");
        p.deviceSetState (device, bytes, length);
    });
}

mdaw_status mdaw_device_reset_state (mdaw_engine* engine, uint64_t device)
{
    return onEngine (engine, "device reset state", [&] (Project& p) { p.deviceResetState (device); });
}

mdaw_status mdaw_device_show_editor (mdaw_engine* engine, uint64_t device, int32_t show, uint64_t token)
{
    return onEngine (engine, show != 0 ? "show editor" : "hide editor", [&] (Project& p) { p.deviceShowEditor (device, show != 0, token); });
}

//==============================================================================
mdaw_status mdaw_plugins_scan (mdaw_engine* engine, const char* format)
{
    return guarded ([&]
    {
        if (engine == nullptr) argumentError ("engine is NULL");
        if (format == nullptr) argumentError ("format is NULL");
        const auto cancelled = [engine] { return engine->scanCancelled.load(); };
        const auto stop = [] { throw Failure (MDAW_ERR_CANCELLED, "plugin scans were cancelled"); };
        if (cancelled()) stop();
        const std::unique_lock scanLock (engine->scanMutex);
        if (cancelled()) stop();

        auto& thread = engine->thread;
        auto& shared = engine->shared;
        shared.scanning = true;
        const struct Done { std::atomic<bool>& flag; ~Done() { flag = false; } } done { shared.scanning };

        // Steps 1 to 5 of the header's description: only the steps in thread.call touch the
        // project; the scanner runs on this thread.
        const auto started = mdaw::bridge::nowMs();
        double onMessageThread = 0;
        const auto onEngineThread = [&] (const char* label, auto fn)
        {
            thread.call ([&]
            {
                const auto t = mdaw::bridge::nowMs();
                fn (*engine->project);
                onMessageThread += mdaw::bridge::nowMs() - t;
            }, label);
        };

        Project::ScanPlan plan;
        onEngineThread ("scan start", [&] (Project& p) { plan = p.scanStart (text (format)); });
        if (! plan.staged)
            return;   // no scanner: scanned in process, all in that one call

        int examined = 0, skipped = 0, failed = 0;
        bool wasCancelled = false;
        std::exception_ptr failure;
        try
        {
            constexpr int chunk = 16;
            for (int from = 0; from < plan.candidates.size() && ! wasCancelled; from += chunk)
            {
                std::vector<Project::ScanDecision> decisions;
                onEngineThread ("scan decide", [&] (Project& p) { decisions = p.scanDecide (plan, from, from + chunk); });
                for (auto& decision : decisions)
                {
                    if (! decision.examine)
                    {
                        ++skipped;
                        continue;
                    }
                    if (cancelled())
                    {
                        wasCancelled = true;
                        break;
                    }
                    const auto result = mdaw::bridge::examineInChild (plan.scanner, plan.format, decision.file, decision.stamp,
                                                                       plan.result, plan.log, plan.timeoutMs, &engine->scanCancelled);
                    if (result.startError.isNotEmpty())
                        throw Failure (MDAW_ERR_INTERNAL, "the plugin scanner could not be started: " + result.startError);
                    if (mdaw::bridge::traceOn())
                        mdaw::bridge::trace ("scan " + plan.format + " " + decision.file + ": scanner ran " + mdaw::bridge::ms (result.childMs)
                                             + " ms, " + (result.cancelled ? juce::String ("cancelled")
                                                          : result.entry.ok ? juce::String ((int) result.found.size()) + " plugin(s)"
                                                                            : result.entry.reason));
                    if (result.cancelled)
                    {
                        wasCancelled = true;
                        break;
                    }
                    ++examined;
                    if (! result.entry.ok)
                        ++failed;
                    onEngineThread ("scan merge", [&] (Project& p) { p.scanMerge (plan.format, decision, result); });
                }
            }
        }
        catch (...)
        {
            failure = std::current_exception();
        }
        wasCancelled = wasCancelled || cancelled();

        // What was taken in is kept, also after a failure or a cancel.
        onEngineThread ("scan finish", [&] (Project& p) { p.scanFinish (plan); });
        if (mdaw::bridge::traceOn())
            mdaw::bridge::trace ("scan " + plan.format + " done: " + juce::String (plan.candidates.size()) + " candidates, "
                                 + juce::String (examined) + " examined, " + juce::String (skipped) + " skipped, "
                                 + juce::String (failed) + " failed" + (wasCancelled ? ", cancelled" : "") + "; "
                                 + mdaw::bridge::ms (mdaw::bridge::nowMs() - started) + " ms, of which "
                                 + mdaw::bridge::ms (onMessageThread) + " ms on the engine's thread");
        if (failure != nullptr)
            std::rethrow_exception (failure);
        if (wasCancelled)
            stop();
    });
}

mdaw_status mdaw_plugins_scan_cancel (mdaw_engine* engine)
{
    if (engine == nullptr)
    {
        setError (MDAW_ERR_ARGUMENT, "engine is NULL");
        return MDAW_ERR_ARGUMENT;
    }
    engine->scanCancelled = true;
    return MDAW_OK;
}

size_t mdaw_plugins_list (mdaw_engine* engine, char* buffer, size_t capacity)
{
    return textFrom (engine, "plugins list", buffer, capacity, [] (Project& p) { return p.pluginsList(); });
}

size_t mdaw_plugins_failed (mdaw_engine* engine, char* buffer, size_t capacity)
{
    return textFrom (engine, "plugins failed", buffer, capacity, [] (Project& p) { return p.pluginsFailed(); });
}

//==============================================================================
mdaw_status mdaw_transport_play (mdaw_engine* engine)
{
    return onEngine (engine, "transport play", [] (Project& p) { p.play(); });
}

mdaw_status mdaw_transport_stop (mdaw_engine* engine)
{
    return onEngine (engine, "transport stop", [] (Project& p) { p.stop(); });
}

mdaw_status mdaw_transport_seek (mdaw_engine* engine, int64_t tick)
{
    return onEngine (engine, "transport seek", [&] (Project& p) { p.seek (tick); });
}

mdaw_status mdaw_transport_set_loop (mdaw_engine* engine, int64_t start_tick, int64_t end_tick)
{
    return onEngine (engine, "transport set loop", [&] (Project& p) { p.setLoop (start_tick, end_tick); });
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
    out->output_latency_seconds = s.outputLatency.load();
    const auto takenAt = s.positionTakenAt.load();
    out->position_age_seconds = takenAt > 0.0 ? juce::jmax (0.0, (juce::Time::getMillisecondCounterHiRes() - takenAt) / 1000.0) : 0.0;
}

size_t mdaw_audio_devices (mdaw_engine* engine, char* buffer, size_t capacity)
{
    return textFrom (engine, "audio devices", buffer, capacity, [] (Project& p) { return p.audioDevices(); });
}

mdaw_status mdaw_audio_device_set (mdaw_engine* engine, const char* backend, const char* output_device,
                                   double sample_rate, int32_t block_size)
{
    return onEngine (engine, "audio device set", [&] (Project& p) { p.audioDeviceSet (text (backend), text (output_device), sample_rate, block_size); });
}

mdaw_status mdaw_render (mdaw_engine* engine, const char* wav_path, int64_t start_tick, int64_t end_tick, double sample_rate)
{
    return onEngine (engine, "render", [&] (Project& p)
    {
        if (wav_path == nullptr || *wav_path == 0) argumentError ("wav_path is empty");
        p.render (fileOf (wav_path), start_tick, end_tick, sample_rate);
    });
}

//==============================================================================
mdaw_status mdaw_test_editor_press_close (mdaw_engine* engine, uint64_t device)
{
    return onEngine (engine, "test press close", [&] (Project& p)
    {
        if (! p.testEditorPressClose (device))
            throw Failure (MDAW_ERR_NOT_FOUND, "the device has no window");
    });
}

int32_t mdaw_test_editor_state (mdaw_engine* engine, uint64_t device, int32_t* visible, uint64_t* token)
{
    bool has = false, shown = false;
    uint64_t t = 0;
    if (onEngine (engine, "test editor state", [&] (Project& p) { has = p.testEditorState (device, shown, t); }) != MDAW_OK)
        return 0;
    if (visible != nullptr) *visible = has && shown ? 1 : 0;
    if (token != nullptr) *token = has ? t : 0;
    return has ? 1 : 0;
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
