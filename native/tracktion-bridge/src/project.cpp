#include "project.h"
#include "clap_format.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <optional>
#include <set>

#if JUCE_LINUX
 #include <dlfcn.h>
#endif

namespace mdaw::bridge
{
namespace
{
constexpr double ticksPerBeat = MDAW_TICKS_PER_QUARTER;

/// Our id of a note, kept on the note's own ValueTree: Tracktion recreates MidiNote objects
/// from the tree, the tree is what lasts.
const juce::Identifier noteIdProperty ("mdawId");

te::BeatPosition beatsOf (int64_t tick)      { return te::BeatPosition::fromBeats ((double) tick / ticksPerBeat); }
te::BeatDuration beatLengthOf (int64_t ticks) { return te::BeatDuration::fromBeats ((double) ticks / ticksPerBeat); }

[[noreturn]] void fail (mdaw_status status, const juce::String& message) { throw Failure (status, message); }

void raise (std::atomic<float>& peak, float value)
{
    auto current = peak.load();
    while (value > current && ! peak.compare_exchange_weak (current, value)) {}
}

/// Tracktion keeps its settings (chosen backend, known plugins, ...) in the bridge's own folder,
/// not in ~/.config/<app>.
struct Storage : te::PropertyStorage
{
    Storage (const juce::String& name, juce::File folderToUse) : te::PropertyStorage (name), folder (std::move (folderToUse)) {}

    juce::File getAppPrefsFolder() override
    {
        folder.createDirectory();
        return folder;
    }

    juce::File getAppCacheFolder() override
    {
        auto cache = folder.getChildFile ("cache");
        cache.createDirectory();
        return cache;
    }

    void flushSettingsToDisk() override { getPropertiesFile().saveIfNeeded(); }

    juce::File folder;
};

struct Behaviour : te::EngineBehaviour
{
    // The bridge opens the device itself, with the backend named explicitly.
    bool autoInitialiseDeviceManager() override { return false; }
    bool shouldOpenAudioInputByDefault() override { return false; }
    // Tracktion's child-process scan starts the current executable again as its worker, and the
    // host executable (the Rust application) does not take part in that. Scans run in process.
    bool canScanPluginsOutOfProcess() override { return false; }
};

/// Keeps a plugin's library mapped for the life of the process. Some plugins leave a thread of
/// their own running after their last instance has gone, and the format unloads the library
/// at that moment, under the thread's feet.
void pinModule (const juce::PluginDescription& desc)
{
   #if JUCE_LINUX
    juce::File bundle (desc.fileOrIdentifier);
    juce::Array<juce::File> libraries;
    if (bundle.isDirectory()) bundle.findChildFiles (libraries, juce::File::findFiles, true, "*.so");
    else if (bundle.existsAsFile()) libraries.add (bundle);
    for (auto& lib : libraries)
        dlopen (lib.getFullPathName().toRawUTF8(), RTLD_LAZY | RTLD_LOCAL | RTLD_NODELETE);
   #else
    juce::ignoreUnused (desc);
   #endif
}

juce::var object (std::initializer_list<std::pair<const char*, juce::var>> properties)
{
    auto* o = new juce::DynamicObject();
    for (auto& [name, value] : properties)
        o->setProperty (name, value);
    return juce::var (o);
}

juce::String json (const juce::var& v) { return juce::JSON::toString (v, true); }
} // namespace

//==============================================================================
void Shared::push (const mdaw_event& e)
{
    const std::scoped_lock lock (mutex);
    for (auto& queued : events)
    {
        if (e.kind == MDAW_EVENT_PARAM_CHANGED && queued.kind == e.kind && queued.device == e.device
            && std::strncmp (queued.param_id, e.param_id, sizeof (e.param_id)) == 0)
        {
            queued.value = e.value;
            return;
        }
        if ((e.kind == MDAW_EVENT_DEVICE_CHANGED || e.kind == MDAW_EVENT_STATE_CHANGED) && queued.kind == e.kind && queued.device == e.device)
            return;
    }
    // Nobody reading: keep the newest.
    if (events.size() >= 4096)
        events.pop_front();
    events.push_back (e);
}

bool Shared::pop (mdaw_event& out)
{
    const std::scoped_lock lock (mutex);
    if (events.empty())
        return false;
    out = events.front();
    events.pop_front();
    return true;
}

//==============================================================================
/// A device on a track: the plugin, and what the bridge watches on it.
struct Project::Device : te::AutomatableParameter::Listener,
                         juce::AudioProcessorListener
{
    Device (Project& p, uint64_t deviceId, uint64_t trackId, te::Plugin::Ptr pluginToUse)
        : owner (p), id (deviceId), track (trackId), plugin (std::move (pluginToUse))
    {
        refreshParams();
        if (auto* pi = instance())
            pi->addListener (this);
    }

    ~Device() override { detach(); }

    void detach()
    {
        if (auto* pi = instance())
            pi->removeListener (this);
        for (auto& p : params)
            p.param->removeListener (this);
        params.clear();
    }

    juce::AudioPluginInstance* instance() const
    {
        if (auto* ext = dynamic_cast<te::ExternalPlugin*> (plugin.get()))
            return ext->getAudioPluginInstance();
        return nullptr;
    }

    /// JUCE's parameter behind one of Tracktion's, for an external plugin. Tracktion names the
    /// parameter by its index unless JUCE's class carries an id.
    juce::AudioProcessorParameter* hostedOf (const te::AutomatableParameter& p) const
    {
        auto* pi = instance();
        if (pi == nullptr)
            return nullptr;
        auto& list = pi->getParameters();
        const auto index = p.paramID.getIntValue();
        if (juce::String (index) == p.paramID && juce::isPositiveAndBelow (index, list.size()))
            return list[index];
        for (auto* hp : list)
            if (auto* withId = dynamic_cast<juce::AudioProcessorParameterWithID*> (hp); withId != nullptr && withId->paramID == p.paramID)
                return hp;
        return nullptr;
    }

    /// The plugin's own parameter id (CLAP id, VST3 ParamID, LV2 symbol), or Tracktion's for
    /// built-in devices.
    juce::String idOf (const te::AutomatableParameter& p) const
    {
        if (auto* hosted = dynamic_cast<juce::HostedAudioProcessorParameter*> (hostedOf (p)))
            return hosted->getParameterID();
        return p.paramID;
    }

    /// Tracktion rebuilds its parameter objects when a plugin is reinitialised; follow it.
    void refreshParams()
    {
        auto* ext = dynamic_cast<te::ExternalPlugin*> (plugin.get());
        std::vector<te::AutomatableParameter*> current;
        for (auto* p : plugin->getAutomatableParameters())
            if (ext == nullptr || (p != ext->dryGain.get() && p != ext->wetGain.get()))   // Tracktion's own, not the plugin's
                current.push_back (p);

        bool same = current.size() == params.size();
        for (size_t i = 0; same && i < current.size(); ++i)
            same = current[i] == params[i].param.get();
        if (same)
            return;

        for (auto& p : params)
            p.param->removeListener (this);
        params.clear();
        for (auto* p : current)
        {
            params.push_back ({ idOf (*p), p });
            p->addListener (this);
            known[params.back().id] = p->getCurrentNormalisedValue();
        }
    }

    te::AutomatableParameter* find (const juce::String& paramId)
    {
        refreshParams();
        for (auto& p : params)
            if (p.id == paramId)
                return p.param.get();
        return nullptr;
    }

    /// How the plugin shows a normalised value of the parameter, with its unit if it gives one
    /// apart.
    juce::String textOf (te::AutomatableParameter& p, float normalised) const
    {
        juce::String text, label;
        if (auto* hosted = hostedOf (p))
        {
            text = hosted->getText (normalised, 1024);
            label = hosted->getLabel();
        }
        else
        {
            text = p.valueToString (p.valueRange.convertFrom0to1 (normalised));
            label = p.getLabel();
        }
        label = label.trim();
        return label.isEmpty() || text.endsWith (label) ? text : text + " " + label;
    }

    void curveHasChanged (te::AutomatableParameter&) override {}
    void parameterChanged (te::AutomatableParameter& p, float) override { owner.parameterMoved (*this, p); }

    // The plugin's own notifications; they may come from any thread, so only a flag is set.
    void audioProcessorParameterChanged (juce::AudioProcessor*, int, float) override {}
    void audioProcessorChanged (juce::AudioProcessor*, const ChangeDetails& details) override
    {
        if (details.programChanged || details.nonParameterStateChanged)
            stateChanged = true;
    }

    struct Param { juce::String id; te::AutomatableParameter::Ptr param; };

    Project& owner;
    const uint64_t id;
    uint64_t track;
    te::Plugin::Ptr plugin;
    std::vector<Param> params;
    std::map<juce::String, float> known;   // last value of each parameter the application knows of
    // What mdaw_device_reset_state goes back to: the plugin's state (external) or the device's
    // tree (built-in) as they were right after the device was made.
    juce::MemoryBlock initialState;
    juce::ValueTree initialTree;
    std::atomic<bool> stateChanged { false };
    juce::uint32 ignoreStateUntil = 0;
};

/// The plugin's own editor in a window with the system's title bar.
struct Project::Editor : juce::DocumentWindow
{
    Editor (const juce::String& title, juce::AudioProcessorEditor* editor)
        : DocumentWindow (title, juce::Colours::black, DocumentWindow::closeButton)
    {
        setUsingNativeTitleBar (true);
        setContentOwned (editor, true);
        centreWithSize (getWidth(), getHeight());
        setVisible (true);
    }

    // The window is deleted by the timer, not from inside its own callback.
    void closeButtonPressed() override
    {
        closedByUser = true;
        setVisible (false);
    }

    bool closedByUser = false;
};

//==============================================================================
Project::Project (const Config& c, Shared& s) : config (c), shared (s)
{
    if (! config.settingsDir.createDirectory())
        fail (MDAW_ERR_IO, "cannot create the settings folder " + config.settingsDir.getFullPathName());

    engine = std::make_unique<te::Engine> (std::make_unique<Storage> (config.appName, config.settingsDir),
                                           std::make_unique<te::UIBehaviour>(),
                                           std::make_unique<Behaviour>());
    // JUCE and Tracktion host no CLAP of their own; this adds it as one more plugin format.
    if (config.enablePlugins)
        engine->getPluginManager().pluginFormatManager.addFormat (mdaw::createClapFormat().release());

    pluginIds = std::make_unique<PluginIds> (config.settingsDir.getChildFile ("plugin-ids.xml"));
    engine->getDeviceManager().addChangeListener (this);
    edit = makeEdit();

    if (config.openDevice)
        openDefaultDevice();

    startTimerHz (30);
}

// Normally reached after shutDownSteps() have run, with only the engine left.
Project::~Project()
{
    stopTimer();
    engine->getDeviceManager().removeChangeListener (this);
    forgetProject();
    if (auto* levels = meteredLevels.get())
        levels->removeClient (meterClient);
    edit.reset();
    retiring.reset();
    engine.reset();
}

Project::Steps Project::shutDownSteps()
{
    return {
        [this]
        {
            stopTimer();
            engine->getDeviceManager().removeChangeListener (this);
            forgetProject();
            retireEdit();
        },
        [this] { retiring.reset(); },
        [this] { engine->getDeviceManager().closeDevices(); }
    };
}

Project::Steps Project::clearSteps()
{
    return {
        [this]
        {
            forgetProject();
            retireEdit();
        },
        [this] { retiring.reset(); },
        [this] { edit = makeEdit(); }
    };
}

/// Drops everything the application's ids point at, and the wish to play.
void Project::forgetProject()
{
    wantPlaying = false;
    offlinePosition = 0;
    seekPending = false;
    shared.playing = 0;
    closeAllEditors();
    for (auto& [id, d] : devices)
        d->detach();
    devices.clear();
    clips.clear();
    tracks.clear();
    tempos.clear();
    loopStart = loopEnd = 0;
}

/// Takes the edit out of use; the next step deletes it.
void Project::retireEdit()
{
    if (edit == nullptr)
        return;
    if (auto* levels = meteredLevels.get())
        levels->removeClient (meterClient);
    meteredLevels = nullptr;
    edit->getTransport().stop (false, true);
    retiring = std::move (edit);   // the timer sees no edit until a new one is made
}

std::unique_ptr<te::Edit> Project::makeEdit()
{
    const auto editFile = config.settingsDir.getChildFile ("bridge.tracktionedit");   // never saved; Tracktion wants a name
    te::Edit::Options options { .engine = *engine,
                                .editState = te::createEmptyEdit (*engine),
                                .editProjectItemID = te::ProjectItemID::createNewID (0) };
    options.role = te::Edit::forEditing;
    options.editFileRetriever = [editFile] { return editFile; };
    options.numAudioTracks = 0;
    options.defaultMasterVolumedB = 0.0f;
    auto e = te::Edit::createEdit (std::move (options));
    if (e == nullptr)
        fail (MDAW_ERR_INTERNAL, "Tracktion could not create an edit");
    e->tempoSequence.getTempo (0)->set (te::BeatPosition(), 120.0, 1.0f, false);
    return e;
}

//==============================================================================
te::AudioTrack* Project::track (uint64_t id) const
{
    auto it = tracks.find (id);
    return it != tracks.end() ? it->second.get() : nullptr;
}

Project::Clip* Project::clip (uint64_t id)
{
    auto it = clips.find (id);
    return it != clips.end() ? &it->second : nullptr;
}

Project::Device* Project::device (uint64_t id) const
{
    auto it = devices.find (id);
    return it != devices.end() ? it->second.get() : nullptr;
}

te::TimePosition Project::timeOf (int64_t tick) const
{
    return edit->tempoSequence.toTime (beatsOf (tick));
}

/// Clips are placed by beat; Tracktion places them by time. So whenever the tempo map changes,
/// every clip is placed again from its ticks.
void Project::placeClip (Clip& c)
{
    c.clip->setPosition ({ { timeOf (c.start), timeOf (c.start + c.length) }, te::TimeDuration() });
}

bool Project::has (int32_t kind, uint64_t id) const
{
    switch (kind)
    {
        case MDAW_ENTITY_TRACK:  return tracks.count (id) > 0;
        case MDAW_ENTITY_CLIP:   return clips.count (id) > 0;
        case MDAW_ENTITY_DEVICE: return devices.count (id) > 0;
        case MDAW_ENTITY_TEMPO:  return tempos.count (id) > 0;
        default:                 fail (MDAW_ERR_ARGUMENT, "unknown entity kind " + juce::String (kind));
    }
}

//==============================================================================
void Project::tempoPut (uint64_t id, int64_t tick, uint32_t milliBpm)
{
    if (id == 0) fail (MDAW_ERR_ARGUMENT, "tempo id 0");
    if (tick < 0) fail (MDAW_ERR_ARGUMENT, "negative tempo position");
    const double bpm = milliBpm / 1000.0;
    if (bpm < te::TempoSetting::minBPM || bpm > te::TempoSetting::maxBPM)
        fail (MDAW_ERR_ARGUMENT, "tempo " + juce::String (bpm, 3) + " bpm is outside the engine's range of "
                                   + juce::String ((int) te::TempoSetting::minBPM) + " to " + juce::String ((int) te::TempoSetting::maxBPM) + " bpm");

    // One point per position: a point put where another one is takes its place.
    for (auto it = tempos.begin(); it != tempos.end();)
        it = (it->first != id && it->second.tick == tick) ? tempos.erase (it) : std::next (it);
    tempos[id] = { tick, milliBpm };
    rebuildTempoMap();
}

void Project::tempoRemove (uint64_t id)
{
    if (tempos.erase (id) > 0)
        rebuildTempoMap();
}

void Project::rebuildTempoMap()
{
    std::vector<Tempo> points;
    double first = 120.0;
    for (auto& [id, t] : tempos)
    {
        if (t.tick == 0) first = t.milliBpm / 1000.0;
        else points.push_back (t);
    }
    std::sort (points.begin(), points.end(), [] (auto& a, auto& b) { return a.tick < b.tick; });

    // Curve 1 is a step: the tempo changes at the point instead of ramping towards it.
    auto& seq = edit->tempoSequence;
    for (int i = seq.getNumTempos(); --i > 0;)
        seq.removeTempo (i, false);
    seq.getTempo (0)->set (te::BeatPosition(), first, 1.0f, false);
    for (auto& t : points)
        seq.insertTempo (beatsOf (t.tick), t.milliBpm / 1000.0, 1.0f);
    seq.updateTempoData();
    edit->getUndoManager().clearUndoHistory();

    for (auto& [id, c] : clips)
        placeClip (c);
    applyLoop();
}

void Project::masterSetGain (float gain)
{
    if (! std::isfinite (gain) || gain < 0.0f) fail (MDAW_ERR_ARGUMENT, "gain must be 0 or more");
    if (auto vol = edit->getMasterVolumePlugin())
        vol->setSliderPos (te::gainToVolumeFaderPosition (gain));
}

//==============================================================================
void Project::trackAdd (uint64_t id, uint64_t after, const juce::String& name)
{
    if (id == 0) fail (MDAW_ERR_ARGUMENT, "track id 0");
    if (tracks.count (id) > 0)
        return;

    // after 0: first. An unknown track to follow: last.
    te::Track* preceding = nullptr;
    if (after != 0)
    {
        preceding = track (after);
        if (preceding == nullptr)
            preceding = te::getAudioTracks (*edit).getLast();
    }
    auto t = edit->insertNewAudioTrack (te::TrackInsertPoint (nullptr, preceding), nullptr);
    if (t == nullptr)
        fail (MDAW_ERR_INTERNAL, "Tracktion could not create a track");
    t->setName (name);
    tracks[id] = t;
}

void Project::trackRemove (uint64_t id)
{
    auto it = tracks.find (id);
    if (it == tracks.end())
        return;

    std::vector<uint64_t> onTrack;
    for (auto& [did, d] : devices)
        if (d->track == id) onTrack.push_back (did);
    for (auto did : onTrack)
        removeDevice (did);
    for (auto c = clips.begin(); c != clips.end();)
        c = c->second.track == id ? clips.erase (c) : std::next (c);

    auto t = it->second;
    tracks.erase (it);
    edit->deleteTrack (t.get());
}

void Project::trackMove (uint64_t id, uint64_t after)
{
    auto* t = track (id);
    if (t == nullptr || after == id)
        return;
    te::Track* preceding = nullptr;
    if (after != 0)
    {
        preceding = track (after);
        if (preceding == nullptr)
            preceding = te::getAudioTracks (*edit).getLast();
        if (preceding == t)
            return;
    }
    edit->moveTrack (t, te::TrackInsertPoint (nullptr, preceding));
}

void Project::trackSetName (uint64_t id, const juce::String& name)
{
    if (auto* t = track (id))
        t->setName (name);
}

void Project::trackSetMix (uint64_t id, float gain, float pan, bool mute)
{
    if (! std::isfinite (gain) || gain < 0.0f) fail (MDAW_ERR_ARGUMENT, "gain must be 0 or more");
    if (! std::isfinite (pan) || pan < -1.0f || pan > 1.0f) fail (MDAW_ERR_ARGUMENT, "pan must be -1 to 1");
    auto* t = track (id);
    if (t == nullptr)
        return;
    // Tracktion's fader works in slider positions over a dB scale; the ABI is linear.
    if (auto* vol = t->getVolumePlugin())
    {
        vol->setSliderPos (te::gainToVolumeFaderPosition (gain));
        vol->setPan (pan);
    }
    t->setMute (mute);
}

//==============================================================================
void Project::clipAdd (uint64_t trackId, uint64_t id, int64_t start, int64_t length, const juce::String& name)
{
    if (id == 0) fail (MDAW_ERR_ARGUMENT, "clip id 0");
    if (start < 0 || length <= 0) fail (MDAW_ERR_ARGUMENT, "clip start must be 0 or more and length more than 0");
    if (clips.count (id) > 0)
        return;
    auto* t = track (trackId);
    if (t == nullptr)
        return;
    auto c = t->insertMIDIClip (name, { timeOf (start), timeOf (start + length) }, nullptr);
    if (c == nullptr)
        fail (MDAW_ERR_INTERNAL, "Tracktion could not create a clip");
    clips[id] = { c, trackId, start, length };
}

void Project::clipRemove (uint64_t id)
{
    auto it = clips.find (id);
    if (it == clips.end())
        return;
    auto c = it->second.clip;
    clips.erase (it);
    c->removeFromParent();
}

void Project::clipSetRange (uint64_t id, int64_t start, int64_t length)
{
    if (start < 0 || length <= 0) fail (MDAW_ERR_ARGUMENT, "clip start must be 0 or more and length more than 0");
    if (auto* c = clip (id))
    {
        c->start = start;
        c->length = length;
        placeClip (*c);
    }
}

void Project::clipSetName (uint64_t id, const juce::String& name)
{
    if (auto* c = clip (id))
        c->clip->setName (name);
}

//==============================================================================
namespace
{
std::map<uint64_t, te::MidiNote*> notesById (te::MidiList& seq)
{
    std::map<uint64_t, te::MidiNote*> notes;
    for (auto* n : seq.getNotes())
        if (n->state.hasProperty (noteIdProperty))
            notes[(uint64_t) (juce::int64) n->state[noteIdProperty]] = n;
    return notes;
}
}

void Project::notesPut (uint64_t clipId, const mdaw_note* notes, size_t count)
{
    // All or nothing: check every note before changing any.
    for (size_t i = 0; i < count; ++i)
    {
        const auto& n = notes[i];
        if (n.id == 0 || n.start_tick < 0 || n.length_ticks <= 0 || n.pitch > 127 || n.velocity < 1 || n.velocity > 127)
            fail (MDAW_ERR_ARGUMENT, "note " + juce::String ((juce::int64) n.id) + ": id must not be 0, start 0 or more, length more than 0, pitch 0..127, velocity 1..127");
    }
    auto* c = clip (clipId);
    if (c == nullptr)
        return;

    auto& seq = c->clip->getSequence();
    auto existing = notesById (seq);
    for (size_t i = 0; i < count; ++i)
    {
        const auto& n = notes[i];
        if (auto it = existing.find (n.id); it != existing.end())
        {
            auto* note = it->second;
            note->setStartAndLength (beatsOf (n.start_tick), beatLengthOf (n.length_ticks), nullptr);
            note->setNoteNumber (n.pitch, nullptr);
            note->setVelocity (n.velocity, nullptr);
        }
        else if (auto* note = seq.addNote (n.pitch, beatsOf (n.start_tick), beatLengthOf (n.length_ticks), n.velocity, 0, nullptr))
        {
            note->state.setProperty (noteIdProperty, (juce::int64) n.id, nullptr);
            existing[n.id] = note;
        }
        else
        {
            fail (MDAW_ERR_INTERNAL, "Tracktion could not add a note");
        }
    }
}

void Project::notesRemove (uint64_t clipId, const uint64_t* ids, size_t count)
{
    auto* c = clip (clipId);
    if (c == nullptr)
        return;
    auto& seq = c->clip->getSequence();
    auto existing = notesById (seq);
    for (size_t i = 0; i < count; ++i)
        if (auto it = existing.find (ids[i]); it != existing.end())
        {
            seq.removeNote (*it->second, nullptr);
            existing.erase (it);
        }
}

//==============================================================================
/// Where a device goes in the track's plugin list: after `after`, and always before the
/// track's own volume and meter plugins.
int Project::insertIndexFor (te::AudioTrack& t, uint64_t after, const te::Plugin* moving) const
{
    auto& list = t.pluginList;
    int end = list.size();
    if (auto* vol = t.getVolumePlugin())
        if (auto i = list.indexOf (vol); i >= 0)
            end = i;
    if (after == 0)
        return 0;
    if (auto* d = device (after); d != nullptr && d->plugin.get() != moving)
        if (auto i = list.indexOf (d->plugin.get()); i >= 0)
            return juce::jmin (i + 1, end);
    return end;
}

void Project::deviceAdd (uint64_t trackId, uint64_t id, uint64_t after, const juce::String& format, const juce::String& identifier)
{
    if (id == 0) fail (MDAW_ERR_ARGUMENT, "device id 0");
    if (devices.count (id) > 0)
        return;
    auto* t = track (trackId);
    if (t == nullptr)
        return;

    te::Plugin::Ptr plugin;
    if (format == "builtin")
    {
        if (identifier != "4osc")
            fail (MDAW_ERR_NOT_FOUND, "no built-in device '" + identifier + "'");
        plugin = edit->getPluginCache().createNewPlugin (te::FourOscPlugin::xmlTypeName, {});
    }
    else
    {
        if (! config.enablePlugins)
            fail (MDAW_ERR_NOT_FOUND, "plugins are switched off in this engine");
        // Tracktion only loads plugins that are in its list of known plugins. The identifier is
        // the plugin's own id (see plugin_ids.h); of several files with the same plugin the
        // first in the list is taken.
        std::optional<juce::PluginDescription> found;
        for (auto& [d, ownId] : loadablePlugins())
            if (d.pluginFormatName == format && ownId == identifier)
            {
                found = d;
                break;
            }
        pluginIds->save();
        if (! found)
            fail (MDAW_ERR_NOT_FOUND, format + " plugin '" + identifier + "' is not installed (or not scanned)");
        pinModule (*found);
        plugin = edit->getPluginCache().createNewPlugin (te::ExternalPlugin::xmlTypeName, *found);
    }
    if (plugin == nullptr)
        fail (MDAW_ERR_PLUGIN, "Tracktion could not create the device");

    t->pluginList.insertPlugin (plugin, insertIndexFor (*t, after, nullptr), nullptr);
    if (t->pluginList.indexOf (plugin.get()) < 0)
        fail (MDAW_ERR_PLUGIN, "the track does not take the device (too many devices?)");

    if (auto* ext = dynamic_cast<te::ExternalPlugin*> (plugin.get()); ext != nullptr && ext->getAudioPluginInstance() == nullptr)
    {
        const auto error = ext->getLoadError();
        plugin->deleteFromParent();
        fail (MDAW_ERR_PLUGIN, "could not load " + identifier + ": " + error);
    }
    auto d = std::make_unique<Device> (*this, id, trackId, plugin);
    // Taken before anything is applied: what mdaw_device_reset_state returns to.
    if (auto* pi = d->instance())
        pi->getStateInformation (d->initialState);
    else
        d->initialTree = plugin->state.createCopy();
    devices[id] = std::move (d);
}

void Project::removeDevice (uint64_t id)
{
    auto it = devices.find (id);
    if (it == devices.end())
        return;
    closeEditor (id);
    auto plugin = it->second->plugin;
    devices.erase (it);
    plugin->deleteFromParent();
}

void Project::deviceRemove (uint64_t id)
{
    removeDevice (id);
}

void Project::deviceMove (uint64_t id, uint64_t after)
{
    auto* d = device (id);
    if (d == nullptr || after == id)
        return;
    auto* t = track (d->track);
    if (t == nullptr)
        return;

    // Moved within the track's state, so the plugin and its instance stay as they are.
    auto& list = t->pluginList;
    auto& tree = d->plugin->state;
    auto parent = tree.getParent();
    const int listIndex = insertIndexFor (*t, after, d->plugin.get());
    const auto* before = list[listIndex];   // what the device goes in front of
    if (before == d->plugin.get())
        return;
    const int from = parent.indexOf (tree);
    int to = before != nullptr ? parent.indexOf (before->state) : parent.indexOf (list[list.size() - 1]->state) + 1;
    if (from < 0 || to < 0)
        return;
    if (from < to) --to;
    if (from != to)
        parent.moveChild (from, to, nullptr);
}

void Project::deviceSetBypass (uint64_t id, bool bypass)
{
    if (auto* d = device (id))
        d->plugin->setEnabled (! bypass);
}

// A value set here reaches an external plugin with the next block it processes (Tracktion
// hands parameters over on the audio thread). Reading the plugin's state straight after a set,
// with nothing processed in between, can still give the old value.
void Project::deviceSetParam (uint64_t id, const juce::String& paramId, float value)
{
    if (! std::isfinite (value) || value < 0.0f || value > 1.0f) fail (MDAW_ERR_ARGUMENT, "parameter value must be 0..1");
    auto* d = device (id);
    if (d == nullptr)
        return;
    auto* p = d->find (paramId);
    if (p == nullptr)
        fail (MDAW_ERR_NOT_FOUND, "the device has no parameter '" + paramId + "'");

    d->known[paramId] = value;
    const juce::ScopedValueSetter<bool> own (ownChange, true);
    p->setNormalisedParameter (value, juce::sendNotification);
    d->known[paramId] = p->getCurrentNormalisedValue();   // stepped parameters snap
}

float Project::deviceGetParam (uint64_t id, const juce::String& paramId)
{
    auto* d = device (id);
    if (d == nullptr)
        fail (MDAW_ERR_NOT_FOUND, "no such device");
    auto* p = d->find (paramId);
    if (p == nullptr)
        fail (MDAW_ERR_NOT_FOUND, "the device has no parameter '" + paramId + "'");
    return p->getCurrentNormalisedValue();
}

juce::String Project::deviceParams (uint64_t id)
{
    auto* d = device (id);
    if (d == nullptr)
        return "[]";
    d->refreshParams();
    juce::Array<juce::var> list;
    for (auto& [pid, param] : d->params)
    {
        auto& p = *param;
        juce::var defaultValue;
        int steps = p.getNumberOfStates();
        double min = 0.0, max = 1.0;
        auto* hosted = d->hostedOf (p);
        if (hosted != nullptr)
        {
            defaultValue = hosted->getDefaultValue();
            if (hosted->isDiscrete() || hosted->isBoolean())
                steps = hosted->getNumSteps();
            mdaw::clapParameterRange (hosted, min, max);   // CLAP has a plain range; others stay 0..1
        }
        else if (auto def = p.getDefaultValue())
        {
            defaultValue = p.valueRange.convertTo0to1 (juce::jlimit (p.valueRange.start, p.valueRange.end, *def));
        }
        list.add (object ({ { "id", pid },
                            { "name", p.getParameterName() },
                            { "value", p.getCurrentNormalisedValue() },
                            { "default", defaultValue },
                            { "steps", steps },
                            { "min", min },
                            { "max", max },
                            { "text", d->textOf (p, p.getCurrentNormalisedValue()) } }));
    }
    return json (list);
}

juce::String Project::deviceParamText (uint64_t id, const juce::String& paramId, float value)
{
    if (! std::isfinite (value) || value < 0.0f || value > 1.0f) fail (MDAW_ERR_ARGUMENT, "parameter value must be 0..1");
    auto* d = device (id);
    if (d == nullptr)
        fail (MDAW_ERR_NOT_FOUND, "no such device");
    auto* p = d->find (paramId);
    if (p == nullptr)
        fail (MDAW_ERR_NOT_FOUND, "the device has no parameter '" + paramId + "'");
    return d->textOf (*p, value);
}

juce::MemoryBlock Project::deviceGetState (uint64_t id)
{
    juce::MemoryBlock block;
    auto* d = device (id);
    if (d == nullptr)
        return block;
    d->plugin->flushPluginStateToValueTree();
    if (auto* pi = d->instance())
    {
        pi->getStateInformation (block);
    }
    else
    {
        juce::MemoryOutputStream out (block, false);
        d->plugin->state.writeToStream (out);
    }
    return block;
}

void Project::deviceSetState (uint64_t id, const void* data, size_t size)
{
    if (auto* d = device (id))
        restoreState (*d, data, size);
}

void Project::deviceResetState (uint64_t id)
{
    auto* d = device (id);
    if (d == nullptr)
        return;
    if (d->instance() != nullptr)
    {
        // A plugin that saves no state has nothing to go back to through it.
        if (d->initialState.isEmpty())
            fail (MDAW_ERR_PLUGIN, "the plugin gave no state when it was loaded, so it cannot be reset");
        restoreState (*d, d->initialState.getData(), d->initialState.getSize());
    }
    else
    {
        juce::MemoryBlock block;
        juce::MemoryOutputStream out (block, false);
        d->initialTree.writeToStream (out);
        out.flush();
        restoreState (*d, block.getData(), block.getSize());
    }
}

void Project::restoreState (Device& device, const void* data, size_t size)
{
    auto* d = &device;
    const juce::ScopedValueSetter<bool> own (ownChange, true);
    d->ignoreStateUntil = juce::Time::getMillisecondCounter() + 500;

    if (auto* pi = d->instance())
    {
        pi->setStateInformation (data, (int) size);
        // Tracktion takes the plugin's new values over asynchronously; take them now, so that
        // reading a parameter straight after this returns the restored value.
        d->refreshParams();
        for (auto& [pid, param] : d->params)
            if (auto* hosted = d->hostedOf (*param))
                param->setParameter (hosted->getValue(), juce::sendNotification);
        d->plugin->flushPluginStateToValueTree();
    }
    else
    {
        auto tree = juce::ValueTree::readFromData (data, size);
        if (! tree.isValid() || ! tree.hasType (d->plugin->state.getType()))
            fail (MDAW_ERR_ARGUMENT, "not a state of this device");
        d->plugin->restorePluginStateFromValueTree (tree);
    }

    d->refreshParams();
    for (auto& [pid, param] : d->params)
        d->known[pid] = param->getCurrentNormalisedValue();
    d->stateChanged = false;
}

void Project::parameterMoved (Device& d, te::AutomatableParameter& p)
{
    if (ownChange)
        return;
    const auto pid = d.idOf (p);
    const auto value = p.getCurrentNormalisedValue();
    // Tracktion echoes values back from the plugin asynchronously; those are known already.
    if (auto it = d.known.find (pid); it != d.known.end() && std::abs (it->second - value) < 1.0e-6f)
        return;
    d.known[pid] = value;

    mdaw_event e {};
    e.kind = MDAW_EVENT_PARAM_CHANGED;
    e.device = d.id;
    e.value = value;
    pid.copyToUTF8 (e.param_id, sizeof (e.param_id));
    shared.push (e);
}

//==============================================================================
void Project::deviceShowEditor (uint64_t id, bool show)
{
    auto* d = device (id);
    if (d == nullptr)
        return;
    if (! show)
    {
        closeEditor (id);
        return;
    }
    if (auto it = editors.find (id); it != editors.end())
    {
        it->second->setVisible (true);
        it->second->toFront (true);
        return;
    }
    auto* pi = d->instance();
    if (pi == nullptr || ! pi->hasEditor())
        fail (MDAW_ERR_NOT_FOUND, "the device has no editor of its own");
    auto* editor = pi->createEditorIfNeeded();
    if (editor == nullptr)
        fail (MDAW_ERR_PLUGIN, "the plugin made no editor");
    editors[id] = std::make_unique<Editor> (d->plugin->getName() + " - " + config.appName, editor);
}

void Project::closeEditor (uint64_t id)
{
    editors.erase (id);
}

void Project::closeAllEditors()
{
    editors.clear();
}

//==============================================================================
void Project::pluginsScan (const juce::String& format)
{
    if (! config.enablePlugins)
        fail (MDAW_ERR_NOT_FOUND, "plugins are switched off in this engine");
    auto& pm = engine->getPluginManager();
    for (auto* fmt : pm.pluginFormatManager.getFormats())
    {
        if (fmt->getName() != format)
            continue;
        // A plugin that takes the process down during a scan is skipped next time.
        const auto pedal = config.settingsDir.getChildFile ("scan-" + format + ".pedal");
        juce::PluginDirectoryScanner scanner (pm.knownPluginList, *fmt, fmt->getDefaultLocationsToSearch(), true, pedal, false);
        juce::String name;
        while (scanner.scanNextFile (true, name)) {}
        engine->getPropertyStorage().flushSettingsToDisk();
        // Reads the own ids of what was found while the libraries are fresh, and keeps them.
        loadablePlugins();
        pluginIds->save();
        return;
    }
    fail (MDAW_ERR_NOT_FOUND, "no plugin format '" + format + "'");
}

std::vector<std::pair<juce::PluginDescription, juce::String>> Project::loadablePlugins()
{
    std::vector<std::pair<juce::PluginDescription, juce::String>> result;
    if (! config.enablePlugins)
        return result;
    auto& pm = engine->getPluginManager();
    std::set<std::pair<juce::String, juce::String>> seen;
    // The remembered list may hold plugins scanned on an earlier run (or with an older bridge):
    // only those whose format is here and whose file is still there are offered, and only under
    // an own id read from the plugin itself.
    for (auto& d : pm.knownPluginList.getTypes())
    {
        if (d.pluginFormatName != "CLAP" && d.pluginFormatName != "VST3" && d.pluginFormatName != "LV2")
            continue;
        juce::AudioPluginFormat* format = nullptr;
        for (auto* f : pm.pluginFormatManager.getFormats())
            if (f->getName() == d.pluginFormatName)
                format = f;
        if (format == nullptr || ! format->doesPluginStillExist (d))
            continue;
        auto ownId = pluginIds->of (d);
        if (ownId.isEmpty() || ! seen.insert ({ d.pluginFormatName, ownId }).second)
            continue;
        result.emplace_back (d, ownId);
    }
    return result;
}

juce::String Project::pluginsList()
{
    juce::Array<juce::var> list;
    for (auto& [d, ownId] : loadablePlugins())
        list.add (object ({ { "format", d.pluginFormatName },
                            { "identifier", ownId },
                            { "name", d.name },
                            { "vendor", d.manufacturerName },
                            { "instrument", d.isInstrument } }));
    pluginIds->save();
    return json (list);
}

//==============================================================================
bool Project::deviceRuns() const
{
    auto* dev = engine->getDeviceManager().deviceManager.getCurrentAudioDevice();
    return dev != nullptr && dev->isOpen();
}

/// Without a running device Tracktion's transport cannot move: it stops itself and takes its
/// position from a playhead that never advances. So playback starts only with a device, and
/// until then the bridge keeps the position itself.
void Project::startTransportIfWanted()
{
    if (! wantPlaying || edit == nullptr || edit->isRendering() || ! deviceRuns())
        return;
    auto& transport = edit->getTransport();
    if (transport.isPlaying())
        return;
    transport.ensureContextAllocated();
    if (std::exchange (seekPending, false))
        transport.setPosition (timeOf (offlinePosition));
    transport.play (false);
}

void Project::play()
{
    wantPlaying = true;
    shared.playing = 1;
    startTransportIfWanted();
}

void Project::stopTransport()
{
    if (edit != nullptr)
        edit->getTransport().stop (false, false);
}

void Project::stop()
{
    wantPlaying = false;
    shared.playing = 0;
    stopTransport();
}

void Project::seek (int64_t tick)
{
    if (tick < 0) fail (MDAW_ERR_ARGUMENT, "negative position");
    const auto t = timeOf (tick);
    edit->getTransport().setPosition (t);
    offlinePosition = tick;
    seekPending = ! deviceRuns();
    shared.positionTick = tick;
    shared.positionSeconds = t.inSeconds();
}

void Project::setLoop (int64_t start, int64_t end)
{
    if (start < 0 || end < 0) fail (MDAW_ERR_ARGUMENT, "negative loop position");
    loopStart = start;
    loopEnd = end;
    applyLoop();
}

void Project::applyLoop()
{
    auto& transport = edit->getTransport();
    if (loopEnd > loopStart)
    {
        transport.setLoopRange ({ timeOf (loopStart), timeOf (loopEnd) });
        transport.looping = true;
    }
    else
    {
        transport.looping = false;
    }
}

//==============================================================================
void Project::initialiseDeviceManager()
{
    if (! std::exchange (deviceManagerInitialised, true))
        engine->getDeviceManager().initialise (0, 2);
}

void Project::openDefaultDevice()
{
    initialiseDeviceManager();
    auto& adm = engine->getDeviceManager().deviceManager;
    // Always name the backend: Tracktion would otherwise reuse whatever its settings remember.
    auto type = config.backend;
    if (type.isEmpty())
        if (auto* first = adm.getAvailableDeviceTypes().getFirst())
            type = first->getTypeName();
    adm.setCurrentAudioDeviceType (type, true);
    auto setup = adm.getAudioDeviceSetup();
    setup.inputDeviceName = {};
    setup.useDefaultInputChannels = false;
    setup.inputChannels.clear();
    setup.useDefaultOutputChannels = true;
    // A device that does not open leaves the engine usable offline; the state says so.
    adm.setAudioDeviceSetup (setup, true);
}

juce::String Project::audioDevices()
{
    auto& adm = engine->getDeviceManager().deviceManager;
    juce::Array<juce::var> backends;
    for (auto* type : adm.getAvailableDeviceTypes())
    {
        type->scanForDevices();
        juce::Array<juce::var> outputs, inputs;
        for (auto& n : type->getDeviceNames (false)) outputs.add (n);
        for (auto& n : type->getDeviceNames (true))  inputs.add (n);
        backends.add (object ({ { "name", type->getTypeName() }, { "outputs", outputs }, { "inputs", inputs } }));
    }
    juce::Array<juce::var> midiInputs;
    for (auto& d : juce::MidiInput::getAvailableDevices())
        midiInputs.add (d.name);

    auto* dev = adm.getCurrentAudioDevice();
    auto current = object ({ { "backend", adm.getCurrentAudioDeviceType() },
                             { "output", dev != nullptr ? dev->getName() : juce::String() },
                             { "sample_rate", dev != nullptr ? dev->getCurrentSampleRate() : 0.0 },
                             { "block_size", dev != nullptr ? dev->getCurrentBufferSizeSamples() : 0 },
                             { "open", dev != nullptr && dev->isOpen() } });
    return json (object ({ { "backends", backends }, { "midi_inputs", midiInputs }, { "current", current } }));
}

void Project::audioDeviceSet (const juce::String& backend, const juce::String& output, double rate, int blockSize)
{
    if (rate < 0 || blockSize < 0) fail (MDAW_ERR_ARGUMENT, "negative sample rate or block size");
    initialiseDeviceManager();
    auto& adm = engine->getDeviceManager().deviceManager;
    if (backend.isNotEmpty() && backend != adm.getCurrentAudioDeviceType())
    {
        bool known = false;
        for (auto* type : adm.getAvailableDeviceTypes())
            known = known || type->getTypeName() == backend;
        if (! known)
            fail (MDAW_ERR_NOT_FOUND, "no audio backend '" + backend + "'");
        adm.setCurrentAudioDeviceType (backend, true);
    }
    auto setup = adm.getAudioDeviceSetup();
    if (output.isNotEmpty()) setup.outputDeviceName = output;
    if (rate > 0) setup.sampleRate = rate;
    if (blockSize > 0) setup.bufferSize = blockSize;
    setup.inputDeviceName = {};
    setup.useDefaultInputChannels = false;
    setup.inputChannels.clear();
    setup.useDefaultOutputChannels = true;
    if (auto error = adm.setAudioDeviceSetup (setup, true); error.isNotEmpty())
        fail (MDAW_ERR_DEVICE, error);
    if (adm.getCurrentAudioDevice() == nullptr)
        fail (MDAW_ERR_DEVICE, "no audio device is open");
}

//==============================================================================
void Project::render (const juce::File& file, int64_t start, int64_t end, double sampleRate)
{
    if (start < 0 || end <= start) fail (MDAW_ERR_ARGUMENT, "render range must be non-empty and not negative");
    if (! (sampleRate >= 8000.0 && sampleRate <= 384000.0)) fail (MDAW_ERR_ARGUMENT, "sample rate out of range");
    if (! file.getParentDirectory().isDirectory()) fail (MDAW_ERR_IO, "no folder " + file.getParentDirectory().getFullPathName());
    if (file.exists() && ! file.deleteFile()) fail (MDAW_ERR_IO, "cannot replace " + file.getFullPathName());

    // The wanted transport state is left as it is: the timer starts playback again afterwards.
    stopTransport();
    juce::String error;
    {
        const te::Edit::ScopedRenderStatus renderStatus (*edit, true);
        te::TransportControl::stopAllTransports (*engine, false, true);
        te::Renderer::turnOffAllPlugins (*edit);

        te::Renderer::Parameters r (*edit);
        r.destFile = file;
        r.audioFormat = engine->getAudioFileFormatManager().getWavFormat();
        r.bitDepth = 24;
        r.sampleRateForAudio = sampleRate;
        r.blockSizeForAudio = 512;
        r.time = { timeOf (start), timeOf (end) };
        r.tracksToDo = te::toBitSet (te::getAllTracks (*edit));
        r.usePlugins = true;
        r.useMasterPlugins = true;
        r.canRenderInMono = false;      // always stereo, as the ABI says
        r.checkNodesForAudio = false;   // an empty or muted project renders silence, not an error

        // Rendered on this thread; the CLAP format treats the rendering thread as the audio thread.
        auto task = te::render_utils::createRenderTask (r, "mdaw render", nullptr, nullptr);
        if (task == nullptr)
            fail (MDAW_ERR_INTERNAL, "Tracktion could not build the render graph");
        while (task->runJob() == juce::ThreadPoolJob::jobNeedsRunningAgain) {}
        error = task->errorMessage;
        task.reset();
        te::Renderer::turnOffAllPlugins (*edit);
    }
    if (error.isNotEmpty())
        fail (MDAW_ERR_IO, error);
    if (! file.existsAsFile())
        fail (MDAW_ERR_IO, "the render wrote no file");
}

//==============================================================================
void Project::changeListenerCallback (juce::ChangeBroadcaster*)
{
    mdaw_event e {};
    e.kind = MDAW_EVENT_DEVICE_CHANGED;
    shared.push (e);
}

void Project::updateDeviceState()
{
    auto& dm = engine->getDeviceManager();
    auto* dev = dm.deviceManager.getCurrentAudioDevice();
    if (dev != countedDevice)
    {
        countedDevice = dev;
        xrunBase = dev != nullptr ? juce::jmax (0, dev->getXRunCount()) : 0;
    }
    const bool open = dev != nullptr && dev->isOpen();
    shared.deviceOpen = open ? 1 : 0;
    shared.sampleRate = open ? dev->getCurrentSampleRate() : 0.0;
    shared.blockSize = open ? dev->getCurrentBufferSizeSamples() : 0;
    shared.xruns = open ? juce::jmax (0, dev->getXRunCount() - xrunBase) : 0;
    shared.cpu = open ? (float) dm.getCpuUsage() : 0.0f;
}

void Project::updateMeter()
{
    auto* context = edit->getCurrentPlaybackContext();
    auto* levels = context != nullptr ? &context->masterLevels : nullptr;
    // The playback context, and its meter, are rebuilt by Tracktion when devices change.
    if (meteredLevels.get() != levels)
    {
        if (auto* old = meteredLevels.get())
            old->removeClient (meterClient);
        meterClient.reset();
        meteredLevels = levels;
        if (levels != nullptr)
            levels->addClient (meterClient);
    }
    if (levels == nullptr)
        return;
    const auto left = juce::Decibels::decibelsToGain (meterClient.getAndClearAudioLevel (0).dB, -100.0f);
    const auto right = meterClient.getNumChannelsUsed() > 1
                         ? juce::Decibels::decibelsToGain (meterClient.getAndClearAudioLevel (1).dB, -100.0f)
                         : left;
    raise (shared.peakLeft, left);
    raise (shared.peakRight, right);
}

void Project::timerCallback()
{
    updateDeviceState();

    // Editors the user closed.
    for (auto it = editors.begin(); it != editors.end();)
    {
        if (it->second->closedByUser)
        {
            mdaw_event e {};
            e.kind = MDAW_EVENT_EDITOR_CLOSED;
            e.device = it->first;
            shared.push (e);
            it = editors.erase (it);
        }
        else
        {
            ++it;
        }
    }

    // Presets and other state changes the plugin announced.
    const auto now = juce::Time::getMillisecondCounter();
    for (auto& [id, d] : devices)
        if (d->stateChanged.exchange (false) && (int) (now - d->ignoreStateUntil) > 0)
        {
            mdaw_event e {};
            e.kind = MDAW_EVENT_STATE_CHANGED;
            e.device = id;
            shared.push (e);
        }

    if (edit == nullptr)
        return;
    auto& transport = edit->getTransport();

    // Tracktion stops the transport whenever the device list changes; the application did not
    // ask for that, so playback is started again.
    startTransportIfWanted();

    const auto position = transport.getPosition();
    if (deviceRuns() && ! seekPending)
    {
        shared.positionSeconds = position.inSeconds();
        shared.positionTick = (int64_t) std::llround (edit->tempoSequence.toBeats (position).inBeats() * ticksPerBeat);
        offlinePosition = shared.positionTick;   // where it stays if the device goes away
    }
    else
    {
        shared.positionSeconds = timeOf (offlinePosition).inSeconds();
        shared.positionTick = offlinePosition;
    }
    updateMeter();

    // The audio callback has stopped returning when the playhead stands still while playing.
    if (shared.deviceOpen.load() != 0 && transport.isPlaying())
    {
        if (position.inSeconds() != lastPosition)
        {
            lastPosition = position.inSeconds();
            positionStillSince = now;
            stallReported = false;
        }
        else if (! stallReported && now - positionStillSince > 1000)
        {
            stallReported = true;
            mdaw_event e {};
            e.kind = MDAW_EVENT_AUDIO_STALLED;
            shared.push (e);
        }
    }
    else
    {
        lastPosition = -1.0;
        positionStillSince = now;
        stallReported = false;
    }
}
} // namespace mdaw::bridge
