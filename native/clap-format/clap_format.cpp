#include "clap_format.h"

#include <juce_gui_extra/juce_gui_extra.h>
#include <clap/clap.h>

#include <dlfcn.h>
#include <map>
#include <unordered_map>

namespace mdaw
{
ClapHostCounters& clapHostCounters() { static ClapHostCounters c; return c; }

namespace
{
//==============================================================================
/// One loaded .clap library. Loaded once and kept for the life of the process: plugins leave
/// threads and static state behind, and unloading under them is a crash the host cannot catch.
struct Module
{
    const clap_plugin_entry_t* entry = nullptr;
    const clap_plugin_factory_t* factory = nullptr;

    static Module* get (const juce::String& path)
    {
        static std::map<juce::String, std::unique_ptr<Module>> modules;
        static juce::CriticalSection lock;
        const juce::ScopedLock sl (lock);
        if (auto found = modules.find (path); found != modules.end())
            return found->second->factory != nullptr ? found->second.get() : nullptr;

        auto module = std::make_unique<Module>();
        if (void* handle = dlopen (path.toRawUTF8(), RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE))
            if (auto* entry = static_cast<const clap_plugin_entry_t*> (dlsym (handle, "clap_entry")))
                if (clap_version_is_compatible (entry->clap_version) && entry->init (path.toRawUTF8()))
                {
                    module->entry = entry;
                    module->factory = static_cast<const clap_plugin_factory_t*> (entry->get_factory (CLAP_PLUGIN_FACTORY_ID));
                }
        auto* result = module->factory != nullptr ? module.get() : nullptr;
        modules[path] = std::move (module);
        return result;
    }
};

bool hasFeature (const clap_plugin_descriptor_t& d, const char* feature)
{
    for (auto f = d.features; f != nullptr && *f != nullptr; ++f)
        if (std::strcmp (*f, feature) == 0) return true;
    return false;
}

int idOf (const clap_plugin_descriptor_t& d)   { return juce::String (juce::CharPointer_UTF8 (d.id)).hashCode(); }

juce::PluginDescription describe (const clap_plugin_descriptor_t& d, const juce::String& path, bool shared)
{
    juce::PluginDescription out;
    out.name = juce::CharPointer_UTF8 (d.name);
    out.descriptiveName = juce::CharPointer_UTF8 (d.description != nullptr ? d.description : "");
    out.pluginFormatName = "CLAP";
    out.isInstrument = hasFeature (d, CLAP_PLUGIN_FEATURE_INSTRUMENT);
    out.category = out.isInstrument ? "Instrument" : "Effect";
    out.manufacturerName = juce::CharPointer_UTF8 (d.vendor != nullptr ? d.vendor : "");
    out.version = juce::CharPointer_UTF8 (d.version != nullptr ? d.version : "");
    out.fileOrIdentifier = path;
    out.lastFileModTime = juce::File (path).getLastModificationTime();
    out.uniqueId = out.deprecatedUid = idOf (d);
    out.numInputChannels = out.isInstrument ? 0 : 2;
    out.numOutputChannels = 2;
    out.hasSharedContainer = shared;
    return out;
}

//==============================================================================
class Instance;

/// What exists before the JUCE object does: the host record the plugin is created with, and
/// the plugin. Host calls reach the instance through `owner` once there is one.
struct Loaded
{
    clap_host_t host {};
    const clap_plugin_t* plugin = nullptr;
    Instance* owner = nullptr;
    std::atomic<bool> callbackWanted { false }, restartWanted { false }, flushWanted { false }, latencyChanged { false };
    juce::PluginDescription description;

    const clap_plugin_params_t* params = nullptr;
    const clap_plugin_state_t* state = nullptr;
    const clap_plugin_latency_t* latency = nullptr;
    const clap_plugin_gui_t* gui = nullptr;
    const clap_plugin_audio_ports_t* audioPorts = nullptr;
    const clap_plugin_note_ports_t* notePorts = nullptr;
    const clap_plugin_timer_support_t* timers = nullptr;
    const clap_plugin_posix_fd_support_t* fds = nullptr;

    struct Port { juce::String name; int channels = 0; };
    std::vector<Port> inputs, outputs;
    uint32_t noteInDialects = 0, noteOutDialects = 0;

    ~Loaded() { stopPluginCallbacks(); if (plugin != nullptr) plugin->destroy (plugin); }

    //==============================================================================
    // Timers and file descriptors the plugin asks the host to watch. A plugin with a toolkit of
    // its own runs its whole event loop from these, and may ask for them while it is still
    // being initialised, so they belong here and not to the JUCE object made afterwards.
    std::map<clap_id, std::unique_ptr<juce::Timer>> pluginTimers;
    clap_id nextTimerId = 1;
    juce::Array<int> registeredFds;

    bool registerTimer (uint32_t periodMs, clap_id* id)
    {
        struct PluginTimer final : juce::Timer
        {
            PluginTimer (Loaded& l, clap_id d) : loaded (l), id (d) {}
            void timerCallback() override
            {
                if (loaded.timers == nullptr) loaded.timers = loaded.extension<clap_plugin_timer_support_t> (CLAP_EXT_TIMER_SUPPORT);
                if (loaded.timers != nullptr) loaded.timers->on_timer (loaded.plugin, id);
            }
            Loaded& loaded; clap_id id;
        };
        *id = nextTimerId++;
        auto timer = std::make_unique<PluginTimer> (*this, *id);
        timer->startTimer ((int) juce::jmax<uint32_t> (1, periodMs));
        pluginTimers[*id] = std::move (timer);
        return true;
    }
    bool unregisterTimer (clap_id id)   { return pluginTimers.erase (id) > 0; }
    bool registerFd (int fd, clap_posix_fd_flags_t flags)
    {
        short mask = 0;
        if (flags & CLAP_POSIX_FD_READ)  mask |= 1;   // POLLIN
        if (flags & CLAP_POSIX_FD_WRITE) mask |= 4;   // POLLOUT
        if (flags & CLAP_POSIX_FD_ERROR) mask |= 8;   // POLLERR
        juce::LinuxEventLoop::registerFdCallback (fd, [this, flags] (int f)
        {
            if (fds == nullptr) fds = extension<clap_plugin_posix_fd_support_t> (CLAP_EXT_POSIX_FD_SUPPORT);
            if (fds != nullptr) fds->on_fd (plugin, f, flags);
        }, mask);
        registeredFds.addIfNotAlreadyThere (fd);
        return true;
    }
    bool unregisterFd (int fd)
    {
        juce::LinuxEventLoop::unregisterFdCallback (fd);
        registeredFds.removeAllInstancesOf (fd);
        return true;
    }
    void stopPluginCallbacks()
    {
        pluginTimers.clear();
        for (int fd : registeredFds) juce::LinuxEventLoop::unregisterFdCallback (fd);
        registeredFds.clear();
    }

    template <typename T> const T* extension (const char* id) const   { return static_cast<const T*> (plugin->get_extension (plugin, id)); }

    void readExtensions()
    {
        params = extension<clap_plugin_params_t> (CLAP_EXT_PARAMS);
        state = extension<clap_plugin_state_t> (CLAP_EXT_STATE);
        latency = extension<clap_plugin_latency_t> (CLAP_EXT_LATENCY);
        gui = extension<clap_plugin_gui_t> (CLAP_EXT_GUI);
        audioPorts = extension<clap_plugin_audio_ports_t> (CLAP_EXT_AUDIO_PORTS);
        notePorts = extension<clap_plugin_note_ports_t> (CLAP_EXT_NOTE_PORTS);
        timers = extension<clap_plugin_timer_support_t> (CLAP_EXT_TIMER_SUPPORT);
        fds = extension<clap_plugin_posix_fd_support_t> (CLAP_EXT_POSIX_FD_SUPPORT);

        if (audioPorts != nullptr)
            for (bool isInput : { true, false })
                for (uint32_t i = 0; i < audioPorts->count (plugin, isInput); ++i)
                {
                    clap_audio_port_info_t info {};
                    if (audioPorts->get (plugin, i, isInput, &info))
                        (isInput ? inputs : outputs).push_back ({ juce::String (juce::CharPointer_UTF8 (info.name)), (int) info.channel_count });
                }
        if (notePorts != nullptr)
            for (bool isInput : { true, false })
                if (notePorts->count (plugin, isInput) > 0)
                {
                    clap_note_port_info_t info {};
                    if (notePorts->get (plugin, 0, isInput, &info))
                        (isInput ? noteInDialects : noteOutDialects) = info.supported_dialects;
                }
    }
};

Loaded& loadedOf (const clap_host_t* host)   { return *static_cast<Loaded*> (host->host_data); }
bool onMainThread()                           { return juce::MessageManager::existsAndIsCurrentThread(); }

/// CLAP's threads are roles. An offline render runs the audio role on whatever thread renders,
/// which may be the message thread; while it does, that thread is the audio thread.
thread_local int audioRoleDepth = 0;
struct AudioRole { AudioRole() { ++audioRoleDepth; } ~AudioRole() { --audioRoleDepth; } };

//==============================================================================
class ClapParameter final : public juce::AudioPluginInstance::HostedParameter
{
public:
    ClapParameter (Loaded& l, const clap_param_info_t& i) : loaded (l), info (i)
    {
        double v = info.default_value;
        loaded.params->get_value (loaded.plugin, info.id, &v);
        value.store (toNormalised (v));
    }

    float toNormalised (double plain) const
    {
        const double span = info.max_value - info.min_value;
        return span > 0.0 ? (float) juce::jlimit (0.0, 1.0, (plain - info.min_value) / span) : 0.0f;
    }
    double toPlain (float normalised) const
    {
        const double plain = info.min_value + (double) normalised * (info.max_value - info.min_value);
        return isStepped() ? std::round (plain) : plain;
    }
    bool isStepped() const                               { return (info.flags & CLAP_PARAM_IS_STEPPED) != 0; }

    float getValue() const override                      { return value.load(); }
    void setValue (float v) override
    {
        value.store (v);
        hostSetAt.store (juce::Time::getMillisecondCounter());
        sendToPlugin.store (true);
        anySendToPlugin->store (true);
    }

    /// Whether a value the plugin reports is its own change rather than an echo of something
    /// the host set. Some plugins (Odin2) report values back some time after the host set them,
    /// and then the one before: taking those would put the parameter back. A change made in
    /// the plugin's own window comes inside a gesture, and is always taken.
    bool takesValueFromPlugin() const
    {
        if (pluginGesture.load() || sendToPlugin.load())
            return pluginGesture.load();
        return juce::Time::getMillisecondCounter() - hostSetAt.load() > echoWindowMs;
    }
    float getDefaultValue() const override               { return toNormalised (info.default_value); }
    juce::String getName (int max) const override        { return juce::String (juce::CharPointer_UTF8 (info.name)).substring (0, max); }
    juce::String getLabel() const override               { return {}; }
    juce::String getParameterID() const override         { return juce::String ((juce::int64) info.id); }
    bool isAutomatable() const override                  { return (info.flags & CLAP_PARAM_IS_AUTOMATABLE) != 0; }
    bool isDiscrete() const override                     { return isStepped(); }
    bool isBoolean() const override                      { return isStepped() && info.max_value - info.min_value == 1.0; }
    int getNumSteps() const override
    {
        return isStepped() ? (int) (info.max_value - info.min_value) + 1 : juce::AudioProcessor::getDefaultNumParameterSteps();
    }
    juce::String getText (float normalised, int max) const override
    {
        char text[256] = {};
        if (onMainThread() && loaded.params->value_to_text (loaded.plugin, info.id, toPlain (normalised), text, sizeof (text)))
            return juce::String (juce::CharPointer_UTF8 (text)).substring (0, max);
        return juce::String (toPlain (normalised), 3);
    }
    float getValueForText (const juce::String& text) const override
    {
        double plain = 0;
        if (onMainThread() && loaded.params->text_to_value (loaded.plugin, info.id, text.toRawUTF8(), &plain))
            return toNormalised (plain);
        return toNormalised (text.getDoubleValue());
    }

    Loaded& loaded;
    const clap_param_info_t info;
    std::atomic<float> value { 0.0f };
    /// Set by the host side, taken by whoever next hands the plugin its events.
    std::atomic<bool> sendToPlugin { false };
    /// Set from the plugin's output events, taken by the message thread to tell listeners.
    std::atomic<bool> changedByPlugin { false };
    std::atomic<int> gesture { 0 };   // 1 = began, 2 = ended, waiting for the message thread
    /// The plugin is inside a gesture on this parameter (between its begin and end events).
    std::atomic<bool> pluginGesture { false };
    /// When the host last set the value (`Time::getMillisecondCounter`).
    std::atomic<juce::uint32> hostSetAt { 0 };
    static constexpr juce::uint32 echoWindowMs = 500;
    std::atomic<bool>* anySendToPlugin = nullptr;
};

//==============================================================================
/// The events of one process() or flush() call, in the plain arrays CLAP wants.
struct EventList
{
    union Event
    {
        clap_event_header_t header;
        clap_event_param_value_t param;
        clap_event_note_t note;
        clap_event_midi_t midi;
        clap_event_transport_t transport;
    };

    std::vector<Event> events;
    clap_input_events_t in {};

    EventList()
    {
        events.reserve (8192);
        in.ctx = this;
        in.size = [] (const clap_input_events_t* l) { return (uint32_t) static_cast<EventList*> (l->ctx)->events.size(); };
        in.get = [] (const clap_input_events_t* l, uint32_t i) -> const clap_event_header_t* { return &static_cast<EventList*> (l->ctx)->events[i].header; };
    }

    Event* add (uint16_t type, uint32_t size, uint32_t time)
    {
        if (events.size() == events.capacity()) return nullptr;   // never allocate on the audio thread
        events.emplace_back();
        auto& e = events.back();
        std::memset (&e, 0, sizeof (e));
        e.header = { size, time, CLAP_CORE_EVENT_SPACE_ID, type, 0 };
        return &e;
    }
};

//==============================================================================
class Instance final : public juce::AudioPluginInstance,
                       private juce::Timer
{
public:
    explicit Instance (std::unique_ptr<Loaded> l)
        : AudioPluginInstance (busesOf (*l)), loaded (std::move (l)), plugin (loaded->plugin)
    {
        loaded->owner = this;
        if (loaded->params != nullptr)
            for (uint32_t i = 0; i < loaded->params->count (plugin); ++i)
            {
                clap_param_info_t info {};
                if (! loaded->params->get_info (plugin, i, &info) || (info.flags & CLAP_PARAM_IS_HIDDEN) != 0)
                    continue;
                auto p = std::make_unique<ClapParameter> (*loaded, info);
                p->anySendToPlugin = &anySendToPlugin;
                byId[info.id] = p.get();
                parameters.push_back (p.get());
                addHostedParameter (std::move (p));
            }
        out.ctx = this;
        out.try_push = [] (const clap_output_events_t* list, const clap_event_header_t* e) { return static_cast<Instance*> (list->ctx)->fromPlugin (*e); };
        startTimerHz (60);
    }

    ~Instance() override
    {
        stopTimer();
        loaded->stopPluginCallbacks();
        releaseResources();
        loaded->owner = nullptr;
    }

    //==============================================================================
    const juce::String getName() const override                    { return loaded->description.name; }
    void fillInPluginDescription (juce::PluginDescription& d) const override
    {
        d = loaded->description;
        d.numInputChannels = getTotalNumInputChannels();
        d.numOutputChannels = getTotalNumOutputChannels();
    }
    bool acceptsMidi() const override                               { return loaded->noteInDialects != 0; }
    bool producesMidi() const override                              { return loaded->noteOutDialects != 0; }
    double getTailLengthSeconds() const override                    { return 0.0; }
    int getNumPrograms() override                                   { return 1; }
    int getCurrentProgram() override                                { return 0; }
    void setCurrentProgram (int) override                           {}
    const juce::String getProgramName (int) override                { return {}; }
    void changeProgramName (int, const juce::String&) override      {}

    bool isBusesLayoutSupported (const BusesLayout& layout) const override
    {
        // The ports are the plugin's; the host may only switch the extra ones off.
        auto fits = [] (const juce::Array<juce::AudioChannelSet>& sets, const std::vector<Loaded::Port>& ports)
        {
            if (sets.size() != (int) ports.size()) return false;
            for (int i = 0; i < sets.size(); ++i)
                if (sets[i].size() != ports[(size_t) i].channels && ! (i > 0 && sets[i].isDisabled()))
                    return false;
            return true;
        };
        return fits (layout.inputBuses, loaded->inputs) && fits (layout.outputBuses, loaded->outputs);
    }

    //==============================================================================
    void prepareToPlay (double sampleRate, int blockSize) override
    {
        if (! onMainThread()) ++clapHostCounters().activationsOffMainThread;
        releaseResources();
        maxBlock = juce::jmax (1, blockSize);
        inScratch.clear(); outScratch.clear(); inPointers.clear(); outPointers.clear();
        for (auto& p : loaded->inputs)  { inScratch.emplace_back (juce::jmax (1, p.channels), maxBlock);  inPointers.emplace_back ((size_t) p.channels); }
        for (auto& p : loaded->outputs) { outScratch.emplace_back (juce::jmax (1, p.channels), maxBlock); outPointers.emplace_back ((size_t) p.channels); }
        inBuffers.assign (loaded->inputs.size(), {});
        outBuffers.assign (loaded->outputs.size(), {});

        active = plugin->activate (plugin, sampleRate, 1, (uint32_t) maxBlock);
        if (active && loaded->latency != nullptr)
            setLatencySamples ((int) loaded->latency->get (plugin));
        loaded->latencyChanged = false;
        loaded->restartWanted = false;
    }

    void releaseResources() override
    {
        const Busy hold (*this, true);
        if (processing) { const AudioRole role; plugin->stop_processing (plugin); processing = false; }
        if (active)     { plugin->deactivate (plugin); active = false; }
    }

    void reset() override
    {
        const Busy hold (*this, false);
        const AudioRole role;
        if (hold.got && active) plugin->reset (plugin);
    }

    void processBlock (juce::AudioBuffer<float>& buffer, juce::MidiBuffer& midi) override
    {
        const int n = buffer.getNumSamples();
        const Busy hold (*this, false);
        const AudioRole role;
        if (! hold.got || ! active || n > maxBlock || (! processing && ! (processing = plugin->start_processing (plugin))))
        {
            // The message thread is flushing parameters or reactivating; silence for one block.
            ++clapHostCounters().blocksSkipped;
            for (int c = getTotalNumInputChannels(); c < buffer.getNumChannels(); ++c) buffer.clear (c, 0, n);
            if (getTotalNumInputChannels() == 0) buffer.clear();
            return;
        }
        ++blocksProcessed;

        // Inputs are copied out first: JUCE hands in and out the same channels, CLAP wants them apart.
        for (size_t port = 0; port < loaded->inputs.size(); ++port)
        {
            auto& scratch = inScratch[port];
            const bool enabled = getBus (true, (int) port) != nullptr && getBus (true, (int) port)->isEnabled();
            for (int c = 0; c < loaded->inputs[port].channels; ++c)
            {
                const int index = enabled ? getChannelIndexInProcessBlockBuffer (true, (int) port, c) : -1;
                if (index >= 0 && index < buffer.getNumChannels()) scratch.copyFrom (c, 0, buffer, index, 0, n);
                else                                               scratch.clear (c, 0, n);
                inPointers[port][(size_t) c] = scratch.getWritePointer (c);
            }
            inBuffers[port] = { inPointers[port].data(), nullptr, (uint32_t) loaded->inputs[port].channels, 0, 0 };
        }
        for (size_t port = 0; port < loaded->outputs.size(); ++port)
        {
            const bool enabled = getBus (false, (int) port) != nullptr && getBus (false, (int) port)->isEnabled();
            for (int c = 0; c < loaded->outputs[port].channels; ++c)
            {
                const int index = enabled ? getChannelIndexInProcessBlockBuffer (false, (int) port, c) : -1;
                outPointers[port][(size_t) c] = index >= 0 && index < buffer.getNumChannels() ? buffer.getWritePointer (index)
                                                                                             : outScratch[port].getWritePointer (c);
            }
            outBuffers[port] = { outPointers[port].data(), nullptr, (uint32_t) loaded->outputs[port].channels, 0, 0 };
        }

        events.events.clear();
        addParameterEvents();
        for (const auto meta : midi)
        {
            const auto m = meta.getMessage();
            const auto time = (uint32_t) juce::jlimit (0, n - 1, meta.samplePosition);
            if ((m.isNoteOn() || m.isNoteOff()) && (loaded->noteInDialects & CLAP_NOTE_DIALECT_CLAP) != 0)
            {
                if (auto* e = events.add (m.isNoteOn() ? CLAP_EVENT_NOTE_ON : CLAP_EVENT_NOTE_OFF, sizeof (clap_event_note_t), time))
                {
                    e->note.note_id = -1;
                    e->note.port_index = 0;
                    e->note.channel = (int16_t) (m.getChannel() - 1);
                    e->note.key = (int16_t) m.getNoteNumber();
                    e->note.velocity = m.getFloatVelocity();
                }
            }
            else if (m.getRawDataSize() <= 3 && (loaded->noteInDialects & CLAP_NOTE_DIALECT_MIDI) != 0)
            {
                if (auto* e = events.add (CLAP_EVENT_MIDI, sizeof (clap_event_midi_t), time))
                    std::memcpy (e->midi.data, m.getRawData(), (size_t) m.getRawDataSize());
            }
        }
        midi.clear();
        midiOut = &midi;

        clap_event_transport_t transport {};
        transport.header = { sizeof (transport), 0, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT, 0 };
        if (auto* head = getPlayHead())
            if (auto pos = head->getPosition())
            {
                if (pos->getIsPlaying())   transport.flags |= CLAP_TRANSPORT_IS_PLAYING;
                if (pos->getIsRecording()) transport.flags |= CLAP_TRANSPORT_IS_RECORDING;
                if (auto bpm = pos->getBpm())             { transport.flags |= CLAP_TRANSPORT_HAS_TEMPO; transport.tempo = *bpm; }
                if (auto secs = pos->getTimeInSeconds())  { transport.flags |= CLAP_TRANSPORT_HAS_SECONDS_TIMELINE; transport.song_pos_seconds = (clap_sectime) std::llround (*secs * (double) CLAP_SECTIME_FACTOR); }
                if (auto beats = pos->getPpqPosition())   { transport.flags |= CLAP_TRANSPORT_HAS_BEATS_TIMELINE; transport.song_pos_beats = (clap_beattime) std::llround (*beats * (double) CLAP_BEATTIME_FACTOR); }
                if (auto sig = pos->getTimeSignature())   { transport.flags |= CLAP_TRANSPORT_HAS_TIME_SIGNATURE; transport.tsig_num = (uint16_t) sig->numerator; transport.tsig_denom = (uint16_t) sig->denominator; }
            }

        clap_process_t process {};
        process.steady_time = steadyTime;
        process.frames_count = (uint32_t) n;
        process.transport = &transport;
        process.audio_inputs = inBuffers.data();
        process.audio_outputs = outBuffers.data();
        process.audio_inputs_count = (uint32_t) inBuffers.size();
        process.audio_outputs_count = (uint32_t) outBuffers.size();
        process.in_events = &events.in;
        process.out_events = &out;
        const auto status = plugin->process (plugin, &process);
        steadyTime += n;
        midiOut = nullptr;
        if (status == CLAP_PROCESS_ERROR)
            for (size_t port = 0; port < outBuffers.size(); ++port)
                for (auto* channel : outPointers[port]) juce::FloatVectorOperations::clear (channel, n);
    }

    //==============================================================================
    void getStateInformation (juce::MemoryBlock& dest) override
    {
        dest.reset();
        if (loaded->state == nullptr) return;
        clap_ostream_t stream {};
        stream.ctx = &dest;
        stream.write = [] (const clap_ostream_t* s, const void* data, uint64_t size) -> int64_t
        {
            static_cast<juce::MemoryBlock*> (s->ctx)->append (data, (size_t) size);
            return (int64_t) size;
        };
        if (! loaded->state->save (plugin, &stream)) dest.reset();
        // What is saved is what the plugin holds, which is not always what it was sent: a
        // plugin may snap or refuse a value. Listeners hear about the difference.
        readValuesFromPlugin();
    }

    void setStateInformation (const void* data, int size) override
    {
        if (loaded->state == nullptr || size <= 0) return;
        juce::MemoryInputStream input (data, (size_t) size, false);
        clap_istream_t stream {};
        stream.ctx = &input;
        stream.read = [] (const clap_istream_t* s, void* buffer, uint64_t wanted) -> int64_t
        {
            return (int64_t) static_cast<juce::MemoryInputStream*> (s->ctx)->read (buffer, (int) juce::jmin<uint64_t> (wanted, 1u << 30));
        };
        const Busy hold (*this, true);
        if (loaded->state->load (plugin, &stream))
            readValuesFromPlugin();
    }

    //==============================================================================
    bool hasEditor() const override
    {
        return loaded->gui != nullptr && loaded->gui->is_api_supported (plugin, CLAP_WINDOW_API_X11, false);
    }
    juce::AudioProcessorEditor* createEditor() override;

    //==============================================================================
    // Called by the plugin through the host record.
    void requestResize (int w, int h);
    static BusesProperties busesOf (const Loaded& l)
    {
        BusesProperties b;
        for (auto& p : l.inputs)  b = b.withInput (p.name, juce::AudioChannelSet::canonicalChannelSet (p.channels), true);
        for (auto& p : l.outputs) b = b.withOutput (p.name, juce::AudioChannelSet::canonicalChannelSet (p.channels), true);
        return b;
    }

    std::unique_ptr<Loaded> loaded;
    const clap_plugin_t* const plugin;
    juce::Component::SafePointer<juce::AudioProcessorEditor> editor;

private:
    /// process() and the message thread's own calls into the plugin must not overlap. The audio
    /// thread never waits for this: if the message thread is in there, it skips the block.
    struct Busy
    {
        Busy (Instance& i, bool wait) : owner (i)
        {
            for (;;)
            {
                bool expected = false;
                if ((got = owner.busy.compare_exchange_strong (expected, true)) || ! wait) break;
                juce::Thread::yield();
            }
        }
        ~Busy() { if (got) owner.busy.store (false); }
        Instance& owner;
        bool got = false;
    };

    void addParameterEvents()
    {
        if (! anySendToPlugin.exchange (false)) return;
        for (auto* p : parameters)
            if (p->sendToPlugin.exchange (false))
                if (auto* e = events.add (CLAP_EVENT_PARAM_VALUE, sizeof (clap_event_param_value_t), 0))
                {
                    e->param.param_id = p->info.id;
                    e->param.cookie = p->info.cookie;
                    e->param.note_id = -1;
                    e->param.port_index = -1;
                    e->param.channel = -1;
                    e->param.key = -1;
                    e->param.value = p->toPlain (p->value.load());
                }
    }

    bool fromPlugin (const clap_event_header_t& e)
    {
        if (e.space_id != CLAP_CORE_EVENT_SPACE_ID) return true;
        if (e.type == CLAP_EVENT_PARAM_VALUE)
        {
            auto& v = reinterpret_cast<const clap_event_param_value_t&> (e);
            if (auto found = byId.find (v.param_id); found != byId.end())
            {
                if (! found->second->takesValueFromPlugin())
                    return true;
                found->second->value.store (found->second->toNormalised (v.value));
                found->second->changedByPlugin.store (true);
                anyChangedByPlugin.store (true);
            }
        }
        else if (e.type == CLAP_EVENT_PARAM_GESTURE_BEGIN || e.type == CLAP_EVENT_PARAM_GESTURE_END)
        {
            auto& g = reinterpret_cast<const clap_event_param_gesture_t&> (e);
            if (auto found = byId.find (g.param_id); found != byId.end())
            {
                found->second->pluginGesture.store (e.type == CLAP_EVENT_PARAM_GESTURE_BEGIN);
                found->second->gesture.store (e.type == CLAP_EVENT_PARAM_GESTURE_BEGIN ? 1 : 2);
                anyChangedByPlugin.store (true);
            }
        }
        else if (e.type == CLAP_EVENT_MIDI && midiOut != nullptr)
        {
            auto& m = reinterpret_cast<const clap_event_midi_t&> (e);
            midiOut->addEvent (m.data, 3, (int) e.time);
        }
        return true;
    }

    void readValuesFromPlugin()
    {
        if (loaded->params == nullptr) return;
        for (auto* p : parameters)
        {
            double v = 0;
            if (loaded->params->get_value (plugin, p->info.id, &v))
            {
                const auto normalised = p->toNormalised (v);
                if (std::abs (normalised - p->value.load()) > 1.0e-7f)
                {
                    if (std::getenv ("MDAW_CLAP_DEBUG") != nullptr)
                        std::fprintf (stderr, "[clap] %s holds %g, host had %g\n", p->info.name, (double) normalised, (double) p->value.load());
                    p->value.store (normalised);
                    p->changedByPlugin.store (true);
                    anyChangedByPlugin.store (true);
                }
            }
        }
    }

    /// The message thread's share: the callbacks the plugin asked for, parameter changes while
    /// no audio is running, and telling listeners what the plugin changed itself.
    void timerCallback() override
    {
        if (loaded->callbackWanted.exchange (false))
            plugin->on_main_thread (plugin);

        const auto blocks = blocksProcessed.load();
        const bool audioRunning = blocks != blocksSeenByTimer;
        blocksSeenByTimer = blocks;
        if (! audioRunning && loaded->params != nullptr && (anySendToPlugin.load() || loaded->flushWanted.exchange (false)))
        {
            const Busy hold (*this, false);
            if (hold.got)
            {
                flushEvents.events.clear();
                std::swap (events.events, flushEvents.events);   // addParameterEvents() fills `events`
                addParameterEvents();
                std::swap (events.events, flushEvents.events);
                // flush() belongs to the audio thread while the plugin is active, even when
                // nothing is processing, and to the main thread otherwise.
                if (active) { const AudioRole role; loaded->params->flush (plugin, &flushEvents.in, &out); }
                else        loaded->params->flush (plugin, &flushEvents.in, &out);
            }
        }

        if (anyChangedByPlugin.exchange (false))
            for (auto* p : parameters)
            {
                if (p->gesture.load() == 1) { p->gesture.store (0); p->beginChangeGesture(); gestureOpen.insert (p); }
                if (p->changedByPlugin.exchange (false)) p->sendValueChangedMessageToListeners (p->value.load());
                if (p->gesture.load() == 2) { p->gesture.store (0); if (gestureOpen.erase (p) > 0) p->endChangeGesture(); }
            }

        if (loaded->latencyChanged.exchange (false) && loaded->latency != nullptr && active)
            setLatencySamples ((int) loaded->latency->get (plugin));
    }

    std::vector<ClapParameter*> parameters;
    std::unordered_map<clap_id, ClapParameter*> byId;
    std::set<ClapParameter*> gestureOpen;
    std::atomic<bool> anySendToPlugin { false }, anyChangedByPlugin { false }, busy { false };
    std::atomic<uint32_t> blocksProcessed { 0 };
    uint32_t blocksSeenByTimer = 0;

    bool active = false, processing = false;
    int maxBlock = 0;
    int64_t steadyTime = 0;
    std::vector<juce::AudioBuffer<float>> inScratch, outScratch;
    std::vector<std::vector<float*>> inPointers, outPointers;
    std::vector<clap_audio_buffer_t> inBuffers, outBuffers;
    EventList events, flushEvents;
    clap_output_events_t out {};
    juce::MidiBuffer* midiOut = nullptr;

};

//==============================================================================
/// The plugin's own window, embedded in a JUCE component the way a VST3 editor is.
class Editor final : public juce::AudioProcessorEditor
{
public:
    explicit Editor (Instance& i) : AudioProcessorEditor (i), instance (i), gui (i.loaded->gui), plugin (i.plugin)
    {
        setOpaque (true);
        addAndMakeVisible (embed);
        created = gui->create (plugin, CLAP_WINDOW_API_X11, false);
        uint32_t w = 640, h = 480;
        if (created) gui->get_size (plugin, &w, &h);
        setResizable (created && gui->can_resize (plugin), false);
        setSize ((int) w, (int) h);
    }

    ~Editor() override
    {
        // Let go of the plugin's window before the plugin destroys it; the embedding would
        // otherwise still talk to a window that is gone, which X answers with a fatal error.
        embed.removeClient();
        if (created)
        {
            gui->hide (plugin);
            gui->destroy (plugin);
        }
    }

    void paint (juce::Graphics& g) override   { g.fillAll (juce::Colours::black); }

    void resized() override
    {
        embed.setBounds (getLocalBounds());
        if (attached && gui->can_resize (plugin))
        {
            auto w = (uint32_t) getWidth(), h = (uint32_t) getHeight();
            gui->adjust_size (plugin, &w, &h);
            gui->set_size (plugin, w, h);
        }
    }

    void parentHierarchyChanged() override
    {
        // The plugin can only be given a parent window once ours is on the screen.
        if (! created || attached || getPeer() == nullptr) return;
        clap_window_t window {};
        window.api = CLAP_WINDOW_API_X11;
        window.x11 = (clap_xwnd) embed.getHostWindowID();
        attached = gui->set_parent (plugin, &window);
        // show() is not counted on: some plugins answer false and are visible all the same.
        const bool shown = attached && gui->show (plugin);
        if (attached) ++clapHostCounters().editorsShown;
        if (std::getenv ("MDAW_CLAP_DEBUG") != nullptr)
            std::fprintf (stderr, "[clap] editor: host window 0x%lx, set_parent %d, show %d\n",
                          (unsigned long) embed.getHostWindowID(), (int) attached, (int) shown);
    }

    bool isAttached() const   { return attached; }

private:
    Instance& instance;
    const clap_plugin_gui_t* gui;
    const clap_plugin_t* plugin;
    juce::XEmbedComponent embed { true, false };
    bool created = false, attached = false;
};

juce::AudioProcessorEditor* Instance::createEditor()
{
    if (! hasEditor()) return nullptr;
    auto* e = new Editor (*this);
    editor = e;
    return e;
}

void Instance::requestResize (int w, int h)
{
    if (editor != nullptr) editor->setSize (w, h);
}

//==============================================================================
// The host side of the CLAP interface.
const void* hostExtension (const clap_host_t*, const char* id)
{
    static const clap_host_thread_check_t threadCheck
    {
        [] (const clap_host_t*) { return onMainThread() && audioRoleDepth == 0; },
        [] (const clap_host_t*) { return audioRoleDepth > 0 || ! onMainThread(); },
    };
    static const clap_host_log_t log
    {
        [] (const clap_host_t* h, clap_log_severity severity, const char* message)
        {
            if (severity >= CLAP_LOG_WARNING)
                std::fprintf (stderr, "[clap %s] %s\n", loadedOf (h).description.name.toRawUTF8(), message);
        },
    };
    static const clap_host_params_t params
    {
        [] (const clap_host_t*, clap_param_rescan_flags) {},
        [] (const clap_host_t*, clap_id, clap_param_clear_flags) {},
        [] (const clap_host_t* h) { loadedOf (h).flushWanted = true; },
    };
    static const clap_host_state_t state           { [] (const clap_host_t*) {} };
    static const clap_host_latency_t latency       { [] (const clap_host_t* h) { loadedOf (h).latencyChanged = true; } };
    static const clap_host_audio_ports_t audioPorts { [] (const clap_host_t*, uint32_t) { return false; }, [] (const clap_host_t*, uint32_t) {} };
    static const clap_host_note_ports_t notePorts
    {
        [] (const clap_host_t*) -> uint32_t { return CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI; },
        [] (const clap_host_t*, uint32_t) {},
    };
    static const clap_host_gui_t gui
    {
        [] (const clap_host_t*) {},
        [] (const clap_host_t* h, uint32_t w, uint32_t height)
        {
            if (auto* owner = loadedOf (h).owner) { owner->requestResize ((int) w, (int) height); return true; }
            return false;
        },
        [] (const clap_host_t*) { return false; },
        [] (const clap_host_t*) { return false; },
        [] (const clap_host_t*, bool) {},
    };
    static const clap_host_timer_support_t timers
    {
        [] (const clap_host_t* h, uint32_t period, clap_id* timerId) { return loadedOf (h).registerTimer (period, timerId); },
        [] (const clap_host_t* h, clap_id timerId)                   { return loadedOf (h).unregisterTimer (timerId); },
    };
    static const clap_host_posix_fd_support_t fds
    {
        [] (const clap_host_t* h, int fd, clap_posix_fd_flags_t flags) { return loadedOf (h).registerFd (fd, flags); },
        [] (const clap_host_t* h, int fd, clap_posix_fd_flags_t flags) { auto& l = loadedOf (h); l.unregisterFd (fd); return l.registerFd (fd, flags); },
        [] (const clap_host_t* h, int fd)                              { return loadedOf (h).unregisterFd (fd); },
    };

    const juce::String wanted (id);
    if (wanted == CLAP_EXT_THREAD_CHECK)     return &threadCheck;
    if (wanted == CLAP_EXT_LOG)              return &log;
    if (wanted == CLAP_EXT_PARAMS)           return &params;
    if (wanted == CLAP_EXT_STATE)            return &state;
    if (wanted == CLAP_EXT_LATENCY)          return &latency;
    if (wanted == CLAP_EXT_AUDIO_PORTS)      return &audioPorts;
    if (wanted == CLAP_EXT_NOTE_PORTS)       return &notePorts;
    if (wanted == CLAP_EXT_GUI)              return &gui;
    if (wanted == CLAP_EXT_TIMER_SUPPORT)    return &timers;
    if (wanted == CLAP_EXT_POSIX_FD_SUPPORT) return &fds;
    return nullptr;
}

std::unique_ptr<Loaded> load (const juce::PluginDescription& description, juce::String& error)
{
    auto* module = Module::get (description.fileOrIdentifier);
    if (module == nullptr) { error = "Not a loadable CLAP library: " + description.fileOrIdentifier; return {}; }

    const clap_plugin_descriptor_t* found = nullptr;
    for (uint32_t i = 0; i < module->factory->get_plugin_count (module->factory); ++i)
        if (auto* d = module->factory->get_plugin_descriptor (module->factory, i))
            if (idOf (*d) == description.uniqueId) found = d;
    if (found == nullptr) { error = "No such plugin in " + description.fileOrIdentifier; return {}; }

    auto loaded = std::make_unique<Loaded>();
    loaded->description = description;
    loaded->host.clap_version = CLAP_VERSION;
    loaded->host.host_data = loaded.get();
    loaded->host.name = "mdaw";
    loaded->host.vendor = "mdaw";
    loaded->host.url = "";
    loaded->host.version = "0";
    loaded->host.get_extension = hostExtension;
    loaded->host.request_restart = [] (const clap_host_t* h) { loadedOf (h).restartWanted = true; };
    loaded->host.request_process = [] (const clap_host_t*) {};
    loaded->host.request_callback = [] (const clap_host_t* h) { loadedOf (h).callbackWanted = true; };

    loaded->plugin = module->factory->create_plugin (module->factory, &loaded->host, found->id);
    if (loaded->plugin == nullptr) { error = "The plugin could not be created"; return {}; }
    if (! loaded->plugin->init (loaded->plugin)) { error = "The plugin failed to initialise"; return {}; }
    loaded->readExtensions();
    return loaded;
}

//==============================================================================
class Format final : public juce::AudioPluginFormat
{
public:
    juce::String getName() const override   { return "CLAP"; }

    void findAllTypesForFile (juce::OwnedArray<juce::PluginDescription>& results, const juce::String& path) override
    {
        if (! fileMightContainThisPluginType (path)) return;
        auto* module = Module::get (path);
        if (module == nullptr) return;
        const auto count = module->factory->get_plugin_count (module->factory);
        for (uint32_t i = 0; i < count; ++i)
            if (auto* d = module->factory->get_plugin_descriptor (module->factory, i))
                results.add (new juce::PluginDescription (describe (*d, path, count > 1)));
    }

    bool fileMightContainThisPluginType (const juce::String& path) override   { return juce::File::isAbsolutePath (path) && juce::File (path).hasFileExtension ("clap") && juce::File (path).existsAsFile(); }
    juce::String getNameOfPluginFromIdentifier (const juce::String& path) override   { return juce::File (path).getFileNameWithoutExtension(); }
    bool pluginNeedsRescanning (const juce::PluginDescription& d) override    { return juce::File (d.fileOrIdentifier).getLastModificationTime() != d.lastFileModTime; }
    bool doesPluginStillExist (const juce::PluginDescription& d) override     { return juce::File (d.fileOrIdentifier).existsAsFile(); }
    bool canScanForPlugins() const override                                  { return true; }
    bool isTrivialToScan() const override                                    { return false; }
    bool requiresUnblockedMessageThreadDuringCreation (const juce::PluginDescription&) const override   { return false; }

    juce::StringArray searchPathsForPlugins (const juce::FileSearchPath& paths, bool recursive, bool) override
    {
        juce::StringArray found;
        for (int i = 0; i < paths.getNumPaths(); ++i)
            for (auto& f : paths[i].findChildFiles (juce::File::findFiles, recursive, "*.clap"))
                found.addIfNotAlreadyThere (f.getFullPathName());
        found.sort (true);
        return found;
    }

    juce::FileSearchPath getDefaultLocationsToSearch() override
    {
        juce::FileSearchPath paths (juce::SystemStats::getEnvironmentVariable ("CLAP_PATH", {}).replace (":", ";"));
        paths.add (juce::File ("~/.clap"));
        paths.add (juce::File ("/usr/lib/clap"));
        paths.add (juce::File ("/usr/local/lib/clap"));
        return paths;
    }

protected:
    void createPluginInstance (const juce::PluginDescription& description, double, int, PluginCreationCallback callback) override
    {
        juce::String error;
        auto loaded = load (description, error);
        callback (loaded != nullptr ? std::make_unique<Instance> (std::move (loaded)) : nullptr, error);
    }
};
} // namespace

std::unique_ptr<juce::AudioPluginFormat> createClapFormat()   { return std::make_unique<Format>(); }

juce::String clapPluginId (const juce::PluginDescription& description)
{
    if (description.pluginFormatName != "CLAP")
        return {};
    auto* module = Module::get (description.fileOrIdentifier);
    if (module == nullptr)
        return {};
    // The description names its plugin by a hash of the id (uniqueId): see describe().
    for (uint32_t i = 0; i < module->factory->get_plugin_count (module->factory); ++i)
        if (auto* d = module->factory->get_plugin_descriptor (module->factory, i))
            if (d->id != nullptr && idOf (*d) == description.uniqueId)
                return juce::String (juce::CharPointer_UTF8 (d->id));
    return {};
}

bool clapParameterRange (const juce::AudioProcessorParameter* parameter, double& min, double& max)
{
    if (auto* p = dynamic_cast<const ClapParameter*> (parameter))
    {
        min = p->info.min_value;
        max = p->info.max_value;
        return true;
    }
    return false;
}
} // namespace mdaw
