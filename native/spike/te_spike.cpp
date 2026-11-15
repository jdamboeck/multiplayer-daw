// Native Tracktion Engine spike for mdaw. A console program with one subcommand per thing to
// prove: devices, playback through a real audio device, save and reload, offline rendering.
// Every subcommand prints `RESULT key=value` lines and exits 0 only if what it checks held.

#include <juce_audio_devices/juce_audio_devices.h>
#include "clap_format.h"
#include "x11_atoms.h"
#include <juce_audio_formats/juce_audio_formats.h>
#include <juce_audio_utils/juce_audio_utils.h>
#include <tracktion_engine/tracktion_engine.h>

#include <cstdio>
#include <iostream>

namespace te = tracktion;
using namespace std::literals;

namespace
{
void say (const juce::String& s) { std::cout << s << std::endl; }
void result (const juce::String& key, const juce::var& value) { std::cout << "RESULT " << key << "=" << value.toString() << std::endl; }

/// Runs the message loop for a while. Tracktion does its housekeeping from timers and async
/// updaters on the message thread, so nothing moves unless this is called.
void pump (int ms) { juce::MessageManager::getInstance()->runDispatchLoopUntil (ms); }

bool scanOutOfProcess = false;

struct Behaviour : te::EngineBehaviour
{
    explicit Behaviour (bool openDevice) : openDevice (openDevice) {}
    bool autoInitialiseDeviceManager() override { return openDevice; }
    bool shouldOpenAudioInputByDefault() override { return false; }
    bool canScanPluginsOutOfProcess() override { return scanOutOfProcess; }
    bool openDevice;
};

std::unique_ptr<te::Engine> makeEngine (bool openDevice)
{
    auto engine = std::make_unique<te::Engine> ("mdaw-te-spike",
                                                std::make_unique<te::UIBehaviour>(),
                                                std::make_unique<Behaviour> (openDevice));
    // JUCE and Tracktion host no CLAP of their own; this adds it as one more plugin format.
    engine->getPluginManager().pluginFormatManager.addFormat (mdaw::createClapFormat().release());
    return engine;
}

juce::String arg (const juce::StringArray& args, const juce::String& name, const juce::String& fallback = {})
{
    auto i = args.indexOf (name);
    return i >= 0 && i + 1 < args.size() ? args[i + 1] : fallback;
}

/// A one-track edit: FourOsc playing a C major arpeggio and a chord over four bars at 120 bpm.
std::unique_ptr<te::Edit> makeEdit (te::Engine& engine, const juce::File& file)
{
    auto edit = te::createEmptyEdit (engine, file);
    edit->tempoSequence.getTempos()[0]->setBpm (120.0);
    edit->ensureNumberOfAudioTracks (1);
    auto track = te::getAudioTracks (*edit)[0];
    track->setName ("Lead");

    auto synth = edit->getPluginCache().createNewPlugin (te::FourOscPlugin::xmlTypeName, {});
    track->pluginList.insertPlugin (synth, 0, nullptr);

    const te::TimeRange range (te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (8.0));
    auto clip = track->insertMIDIClip ("Arp", range, nullptr);
    auto& seq = clip->getSequence();
    const int pitches[] = { 60, 64, 67, 72, 67, 64, 60, 55 };
    for (int i = 0; i < 8; ++i)
        seq.addNote (pitches[i], te::BeatPosition::fromBeats (i), te::BeatDuration::fromBeats (0.9), 100, 0, nullptr);
    for (int p : { 48, 55, 64 })
        seq.addNote (p, te::BeatPosition::fromBeats (8.0), te::BeatDuration::fromBeats (8.0), 90, 0, nullptr);
    return edit;
}

int countNotes (te::Edit& edit)
{
    int n = 0;
    for (auto t : te::getAudioTracks (edit))
        for (auto c : t->getClips())
            if (auto m = dynamic_cast<te::MidiClip*> (c))
                n += m->getSequence().getNotes().size();
    return n;
}

struct Level { double peak = 0, rms = 0, seconds = 0; int channels = 0; double rate = 0; };

Level measure (const juce::File& file)
{
    juce::AudioFormatManager formats;
    formats.registerBasicFormats();
    Level l;
    if (auto reader = std::unique_ptr<juce::AudioFormatReader> (formats.createReaderFor (file)))
    {
        juce::AudioBuffer<float> buf ((int) reader->numChannels, (int) reader->lengthInSamples);
        reader->read (&buf, 0, buf.getNumSamples(), 0, true, true);
        l.peak = buf.getMagnitude (0, buf.getNumSamples());
        double sum = 0;
        for (int c = 0; c < buf.getNumChannels(); ++c)
        {
            auto r = buf.getRMSLevel (c, 0, buf.getNumSamples());
            sum += r * r;
        }
        l.rms = std::sqrt (sum / juce::jmax (1, buf.getNumChannels()));
        l.seconds = (double) reader->lengthInSamples / reader->sampleRate;
        l.channels = (int) reader->numChannels;
        l.rate = reader->sampleRate;
    }
    return l;
}

//==============================================================================
int cmdDevices()
{
    auto engine = makeEngine (false);
    auto& adm = engine->getDeviceManager().deviceManager;
    juce::OwnedArray<juce::AudioIODeviceType> types;
    adm.createAudioDeviceTypes (types);
    int outputs = 0;
    for (auto type : types)
    {
        type->scanForDevices();
        auto outs = type->getDeviceNames (false);
        auto ins = type->getDeviceNames (true);
        say ("type " + type->getTypeName() + ": " + juce::String (outs.size()) + " outputs, " + juce::String (ins.size()) + " inputs");
        for (auto& n : outs) say ("  out: " + n);
        for (auto& n : ins)  say ("  in:  " + n);
        outputs += outs.size();
    }
    auto midiIns = juce::MidiInput::getAvailableDevices();
    auto midiOuts = juce::MidiOutput::getAvailableDevices();
    for (auto& d : midiIns)  say ("midi in:  " + d.name + " [" + d.identifier + "]");
    for (auto& d : midiOuts) say ("midi out: " + d.name + " [" + d.identifier + "]");
    result ("device_types", types.size());
    result ("audio_outputs", outputs);
    result ("midi_inputs", midiIns.size());
    return outputs > 0 ? 0 : 1;
}

/// Opens the device as asked. Returns an error text, empty on success.
juce::String openDevice (te::Engine& engine, const juce::StringArray& args)
{
    auto& dm = engine.getDeviceManager();
    auto& adm = dm.deviceManager;
    dm.initialise (0, 2);
    // Always say which backend: Tracktion remembers the last one in its settings file, and a
    // test must not depend on the run before it.
    adm.setCurrentAudioDeviceType (arg (args, "--type", "ALSA"), true);
    auto setup = adm.getAudioDeviceSetup();
    auto device = arg (args, "--device");
    if (device.isNotEmpty()) setup.outputDeviceName = device;
    if (auto rate = arg (args, "--rate").getDoubleValue(); rate > 0) setup.sampleRate = rate;
    if (auto block = arg (args, "--block").getIntValue(); block > 0) setup.bufferSize = block;
    setup.inputDeviceName = {};
    setup.useDefaultInputChannels = false;
    setup.inputChannels.clear();
    auto error = adm.setAudioDeviceSetup (setup, true);
    pump (300);
    if (error.isNotEmpty()) return error;
    if (adm.getCurrentAudioDevice() == nullptr) return "no audio device is open";
    return {};
}

/// Plays for `ms`, sampling the output level. Tracktion stops every transport when its device
/// list changes (it polls MIDI devices every four seconds, and opening our own MIDI ports is
/// itself such a change), so whoever wants playback has to say so again; `restarts` counts it.
struct Playing { double peak = 0; int restarts = 0; };

Playing keepPlaying (te::Edit& edit, juce::AudioDeviceManager& adm, int ms)
{
    auto& transport = edit.getTransport();
    auto level = adm.getOutputLevelGetter();
    Playing p;
    for (int waited = 0; waited < ms; waited += 20)
    {
        pump (20);
        p.peak = juce::jmax (p.peak, level->getCurrentLevel());
        if (! transport.isPlaying())
        {
            transport.play (false);
            ++p.restarts;
        }
    }
    return p;
}

int cmdPlay (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    if (auto error = openDevice (*engine, args); error.isNotEmpty())
    {
        say ("could not open the device: " + error);
        return 1;
    }
    auto& adm = engine->getDeviceManager().deviceManager;
    auto* device = adm.getCurrentAudioDevice();
    result ("device_type", device->getTypeName());
    result ("device", device->getName());
    result ("sample_rate", device->getCurrentSampleRate());
    result ("block", device->getCurrentBufferSizeSamples());
    result ("out_channels", device->getActiveOutputChannels().countNumberOfSetBits());
    result ("out_latency_samples", device->getOutputLatencyInSamples());
    const int xrunsBefore = device->getXRunCount();

    auto file = juce::File::createTempFile (".tracktionedit");
    auto edit = makeEdit (*engine, file);
    auto& transport = edit->getTransport();
    auto level = adm.getOutputLevelGetter();

    // Play from the start for two seconds.
    transport.ensureContextAllocated();
    transport.setPosition (te::TimePosition::fromSeconds (0.0));
    transport.play (false);
    const auto first = keepPlaying (*edit, adm, 2000);
    const bool playing1 = transport.isPlaying();
    const double pos1 = transport.getPosition().inSeconds();

    // Stop: the playhead stays, the output falls silent once the release tails are over.
    transport.stop (false, false);
    pump (1500);
    const bool stopped = ! transport.isPlaying();
    const double posStopped = transport.getPosition().inSeconds();
    double peakStopped = 0;
    for (int i = 0; i < 5; ++i) { pump (100); peakStopped = juce::jmax (peakStopped, level->getCurrentLevel()); }

    // Seek into the chord and start again.
    transport.setPosition (te::TimePosition::fromSeconds (5.0));
    pump (100);
    const double posSeek = transport.getPosition().inSeconds();
    transport.play (false);
    const auto second = keepPlaying (*edit, adm, 700);

    // Edit while it plays: a note, and a whole new track with its own instrument. Playback has
    // to carry on, which is what applying remote edits during local playback needs.
    auto tracks = te::getAudioTracks (*edit);
    dynamic_cast<te::MidiClip*> (tracks[0]->getClips()[0])->getSequence()
        .addNote (79, te::BeatPosition::fromBeats (12.0), te::BeatDuration::fromBeats (2.0), 110, 0, nullptr);
    edit->ensureNumberOfAudioTracks (2);
    auto added = te::getAudioTracks (*edit)[1];
    added->pluginList.insertPlugin (edit->getPluginCache().createNewPlugin (te::FourOscPlugin::xmlTypeName, {}), 0, nullptr);
    added->insertMIDIClip ("Bass", { te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (8.0) }, nullptr)
        ->getSequence().addNote (36, te::BeatPosition::fromBeats (8.0), te::BeatDuration::fromBeats (8.0), 110, 0, nullptr);
    const auto third = keepPlaying (*edit, adm, 800);
    const bool playing2 = transport.isPlaying();
    const double pos2 = transport.getPosition().inSeconds();
    transport.stop (false, false);
    pump (300);

    result ("playing_after_start", playing1 ? 1 : 0);
    result ("position_after_2s", pos1);
    result ("peak_while_playing", first.peak);
    result ("restarts_after_device_open", first.restarts);
    result ("stopped", stopped ? 1 : 0);
    result ("position_when_stopped", posStopped);
    result ("peak_when_stopped", peakStopped);
    result ("position_after_seek", posSeek);
    result ("playing_after_edits", playing2 ? 1 : 0);
    result ("restarts_during_edits", second.restarts + third.restarts);
    result ("position_after_seek_play_1_5s", pos2);
    result ("peak_after_seek", juce::jmax (second.peak, third.peak));
    result ("xruns", device->getXRunCount() - xrunsBefore);
    result ("cpu", engine->getDeviceManager().getCpuUsage());

    const bool ok = playing1 && pos1 > 1.5 && pos1 < 2.6 && first.peak > 0.01
                    && stopped && std::abs (posStopped - pos1) < 0.2 && peakStopped < 0.001
                    && std::abs (posSeek - 5.0) < 0.05 && playing2 && second.restarts + third.restarts == 0
                    && pos2 > 6.0 && pos2 < 7.0 && second.peak > 0.01;
    edit.reset();
    file.deleteFile();
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}

int cmdSaveLoad (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    juce::File file (juce::File::getCurrentWorkingDirectory().getChildFile (arg (args, "--file", "spike.tracktionedit")));
    file.deleteFile();
    int notesBefore = 0;
    {
        auto edit = makeEdit (*engine, file);
        // A changed synth parameter has to survive the round trip too.
        auto synth = te::getAudioTracks (*edit)[0]->pluginList.findFirstPluginOfType<te::FourOscPlugin>();
        synth->ampAttack->setParameter (0.25f, juce::dontSendNotification);
        edit->tempoSequence.insertTempo (te::BeatPosition::fromBeats (8.0), 90.0, 0.0f);
        notesBefore = countNotes (*edit);
        const bool saved = te::EditFileOperations (*edit).save (false, true, false);
        result ("saved", saved ? 1 : 0);
        if (! saved) return 1;
    }
    result ("file_bytes", (int) file.getSize());
    auto edit = te::loadEditFromFile (*engine, file);
    if (edit == nullptr) { say ("reload failed"); return 1; }
    auto tracks = te::getAudioTracks (*edit);
    const int notesAfter = countNotes (*edit);
    auto synth = tracks.isEmpty() ? nullptr : tracks[0]->pluginList.findFirstPluginOfType<te::FourOscPlugin>();
    result ("tracks", tracks.size());
    result ("track_name", tracks.isEmpty() ? juce::String() : tracks[0]->getName());
    result ("notes_before", notesBefore);
    result ("notes_after", notesAfter);
    result ("tempos", edit->tempoSequence.getNumTempos());
    result ("second_tempo_bpm", edit->tempoSequence.getNumTempos() > 1 ? edit->tempoSequence.getTempo (1)->getBpm() : 0.0);
    result ("synth_restored", synth != nullptr ? 1 : 0);
    result ("synth_attack", synth != nullptr ? (double) synth->ampAttack->getCurrentValue() : -1.0);
    const bool ok = tracks.size() == 1 && notesAfter == notesBefore && notesAfter == 11
                    && edit->tempoSequence.getNumTempos() == 2 && synth != nullptr
                    && std::abs (synth->ampAttack->getCurrentValue() - 0.25f) < 1.0e-4f;
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}

/// Renders all tracks of the edit to a wav file without a device. Returns false if it failed.
bool renderEdit (te::Edit& edit, const juce::File& out, double seconds)
{
    out.deleteFile();
    juce::BigInteger tracks;
    tracks.setRange (0, te::getAllTracks (edit).size(), true);
    const te::TimeRange range (te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (seconds));
    return te::Renderer::renderToFile ("spike render", out, edit, range, tracks, true, true, {}, false);
}

int cmdRender (const juce::StringArray& args)
{
    // No audio device is opened in this process at all.
    auto engine = makeEngine (false);
    juce::File out (juce::File::getCurrentWorkingDirectory().getChildFile (arg (args, "--out", "spike.wav")));
    auto file = juce::File::createTempFile (".tracktionedit");
    auto edit = makeEdit (*engine, file);

    const auto started = juce::Time::getMillisecondCounterHiRes();
    const bool rendered = renderEdit (*edit, out, 9.0);
    const auto took = juce::Time::getMillisecondCounterHiRes() - started;
    auto level = measure (out);
    result ("device_open", engine->getDeviceManager().deviceManager.getCurrentAudioDevice() != nullptr ? 1 : 0);
    result ("rendered", rendered ? 1 : 0);
    result ("render_ms", took);
    result ("seconds", level.seconds);
    result ("channels", level.channels);
    result ("sample_rate", level.rate);
    result ("peak", level.peak);
    result ("rms", level.rms);
    const bool ok = rendered && level.seconds > 8.9 && level.peak > 0.05 && level.rms > 0.005
                    && engine->getDeviceManager().deviceManager.getCurrentAudioDevice() == nullptr;
    edit.reset();
    file.deleteFile();
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}
} // namespace

#include "plugins.inc"

int main (int argc, char** argv)
{
    mdaw::defineWindowManagerAtoms();
    juce::ScopedJuceInitialiser_GUI juce;
    juce::StringArray args (argv + 1, argc - 1);
    // Tracktion's out-of-process plugin scan starts this same program again as its worker.
    if (te::PluginManager::startChildProcessPluginScan (args.joinIntoString (" ")))
    {
        juce::MessageManager::getInstance()->runDispatchLoop();
        return 0;
    }
    const auto cmd = args[0];
    scanOutOfProcess = args.contains ("--oop");
    result ("tracktion_engine", te::Engine::getVersion());
    result ("juce", juce::SystemStats::getJUCEVersion());

    if (cmd == "devices")  return cmdDevices();
    if (cmd == "play")     return cmdPlay (args);
    if (cmd == "saveload") return cmdSaveLoad (args);
    if (cmd == "render")   return cmdRender (args);
    if (auto code = pluginCommands (cmd, args); code >= 0) return code;

    std::cerr << "usage: mdaw-te-spike devices | play [--type T] [--device D] [--rate R] [--block B]\n"
                 "       | saveload [--file F] | render [--out F] | plugin commands (see plugins.inc)\n";
    return 2;
}
