// The engine side of the bridge: the Tracktion Engine, the one Edit derived from the
// application's project, and the maps from the application's ids to Tracktion's objects.
// Everything here runs on the engine's message thread, except Shared, which any thread reads.
#pragma once

#include "mdaw_engine.h"
#include "plugin_ids.h"
#include "plugin_scan.h"

#include <juce_audio_processors/juce_audio_processors.h>
#include <tracktion_engine/tracktion_engine.h>

#include <atomic>
#include <deque>
#include <functional>
#include <map>
#include <mutex>
#include <stdexcept>

namespace mdaw::bridge
{
namespace te = tracktion;

/// An error on the message thread that becomes a status code at the C boundary.
struct Failure : std::runtime_error
{
    Failure (mdaw_status s, const juce::String& message) : std::runtime_error (message.toStdString()), status (s) {}
    mdaw_status status;
};

/// What the application reads without waiting for the message thread: state kept current by a
/// timer, and the event queue.
struct Shared
{
    std::atomic<int32_t> playing { 0 }, deviceOpen { 0 }, blockSize { 0 }, xruns { 0 };
    std::atomic<int64_t> positionTick { 0 };
    std::atomic<double> positionSeconds { 0 }, sampleRate { 0 };
    std::atomic<float> cpu { 0 }, peakLeft { 0 }, peakRight { 0 };

    /// Adds an event. A parameter change replaces one still queued for the same parameter, and a
    /// device change one still queued, so a dragged knob does not flood the queue.
    void push (const mdaw_event&);
    bool pop (mdaw_event&);

private:
    std::mutex mutex;
    std::deque<mdaw_event> events;
};

struct Config
{
    juce::String appName;
    juce::File settingsDir;
    bool openDevice = false;
    juce::String backend;
    bool enablePlugins = false;
    juce::File scanner;   // mdaw-plugin-scan; none: plugins are examined in this process
};

class Project : private juce::Timer,
                private juce::ChangeListener
{
public:
    Project (const Config&, Shared&);
    ~Project() override;

    using Steps = std::vector<std::function<void()>>;
    /// Edits are destroyed with the message loop running before and after (Tracktion and
    /// plugins, VST3 in particular, finish their teardown in posted messages), so these are
    /// steps for EngineThread::call (steps, gapMs) with that gap between them.
    static constexpr int teardownGapMs = 300;
    Steps clearSteps();
    /// Stops playback, closes editors, drops the edit and closes the device. What is left for
    /// the destructor is the engine.
    Steps shutDownSteps();

    bool has (int32_t kind, uint64_t id) const;

    void tempoPut (uint64_t id, int64_t tick, uint32_t milliBpm);
    void tempoRemove (uint64_t id);
    void masterSetGain (float gain);

    void trackAdd (uint64_t id, uint64_t after, const juce::String& name);
    void trackRemove (uint64_t id);
    void trackMove (uint64_t id, uint64_t after);
    void trackSetName (uint64_t id, const juce::String& name);
    void trackSetMix (uint64_t id, float gain, float pan, bool mute);

    void clipAdd (uint64_t track, uint64_t id, int64_t start, int64_t length, const juce::String& name);
    void clipRemove (uint64_t id);
    void clipSetRange (uint64_t id, int64_t start, int64_t length);
    void clipSetName (uint64_t id, const juce::String& name);

    void notesPut (uint64_t clip, const mdaw_note* notes, size_t count);
    void notesRemove (uint64_t clip, const uint64_t* ids, size_t count);

    void deviceAdd (uint64_t track, uint64_t id, uint64_t after, const juce::String& format, const juce::String& identifier);
    void deviceRemove (uint64_t id);
    void deviceMove (uint64_t id, uint64_t after);
    void deviceSetBypass (uint64_t id, bool bypass);
    void deviceSetParam (uint64_t id, const juce::String& param, float value);
    float deviceGetParam (uint64_t id, const juce::String& param);
    juce::String deviceParams (uint64_t id);
    juce::String deviceParamText (uint64_t id, const juce::String& param, float value);
    juce::MemoryBlock deviceGetState (uint64_t id);
    void deviceSetState (uint64_t id, const void* data, size_t size);
    void deviceResetState (uint64_t id);
    void deviceShowEditor (uint64_t id, bool show);

    void pluginsScan (const juce::String& format);
    juce::String pluginsList();
    juce::String pluginsFailed();

    void play();
    void stop();
    void seek (int64_t tick);
    void setLoop (int64_t start, int64_t end);

    juce::String audioDevices();
    void audioDeviceSet (const juce::String& backend, const juce::String& output, double rate, int blockSize);

    void render (const juce::File&, int64_t start, int64_t end, double sampleRate);

private:
    struct Device;
    struct Editor;
    struct Clip
    {
        juce::ReferenceCountedObjectPtr<te::MidiClip> clip;
        uint64_t track = 0;
        int64_t start = 0, length = 0;
    };
    struct Tempo { int64_t tick = 0; uint32_t milliBpm = 120000; };

    Config config;
    Shared& shared;
    std::unique_ptr<te::Engine> engine;
    std::unique_ptr<te::Edit> edit;
    std::unique_ptr<te::Edit> retiring;   // between the steps that drop an edit
    std::unique_ptr<PluginIds> pluginIds;
    std::unique_ptr<ScanRecords> scanRecords;

    std::map<uint64_t, juce::ReferenceCountedObjectPtr<te::AudioTrack>> tracks;
    std::map<uint64_t, Clip> clips;
    std::map<uint64_t, std::unique_ptr<Device>> devices;
    std::map<uint64_t, Tempo> tempos;
    std::map<uint64_t, std::unique_ptr<Editor>> editors;

    bool wantPlaying = false;
    bool deviceManagerInitialised = false;
    bool ownChange = false;   // set while the application's own call changes parameters
    int64_t loopStart = 0, loopEnd = 0;
    int64_t offlinePosition = 0;          // where the transport is while no device runs it
    bool seekPending = false;             // a seek made with no device, for when one opens

    // Timer state
    te::LevelMeasurer::Client meterClient;
    juce::WeakReference<te::LevelMeasurer> meteredLevels;
    juce::AudioIODevice* countedDevice = nullptr;
    int xrunBase = 0;
    double lastPosition = -1.0;
    juce::uint32 positionStillSince = 0;
    bool stallReported = false;

    std::unique_ptr<te::Edit> makeEdit();
    void forgetProject();
    void retireEdit();
    void openDefaultDevice();
    void initialiseDeviceManager();

    te::AudioTrack* track (uint64_t id) const;
    Clip* clip (uint64_t id);
    Device* device (uint64_t id) const;
    te::TimePosition timeOf (int64_t tick) const;
    void placeClip (Clip&);
    void rebuildTempoMap();
    void applyLoop();
    void stopTransport();
    bool deviceRuns() const;
    void startTransportIfWanted();
    void removeDevice (uint64_t id);
    void restoreState (Device&, const void* data, size_t size);
    /// The plugins that can be loaded now, each (format, own id) once: the first description
    /// in the known-plugin list wins.
    std::vector<std::pair<juce::PluginDescription, juce::String>> loadablePlugins();
    bool usesScanner() const { return config.scanner != juce::File(); }
    void scanInProcess (juce::AudioPluginFormat&);
    void scanWithScanner (juce::AudioPluginFormat&);
    void examineInChild (juce::AudioPluginFormat&, const juce::String& file, juce::int64 stamp,
                         const juce::File& result, const juce::File& log, int timeoutMs);
    juce::int64 candidateStamp (juce::AudioPluginFormat&, const juce::String& file);
    void closeEditor (uint64_t id);
    void closeAllEditors();
    int insertIndexFor (te::AudioTrack&, uint64_t after, const te::Plugin* moving) const;

    void timerCallback() override;
    void changeListenerCallback (juce::ChangeBroadcaster*) override;
    void updateMeter();
    void updateDeviceState();

    void parameterMoved (Device&, te::AutomatableParameter&);
};
} // namespace mdaw::bridge
