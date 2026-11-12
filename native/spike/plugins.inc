#include <dlfcn.h>
static bool args_noPin = false;
// Plugin hosting checks of the spike (phase 2): scanning, loading, audio, parameters, state,
// automation and editor windows for the plugin formats JUCE hosts.
namespace
{
juce::Array<juce::PluginDescription> typesForFile (te::Engine& engine, const juce::String& path, const juce::String& format)
{
    juce::Array<juce::PluginDescription> found;
    for (auto fmt : engine.getPluginManager().pluginFormatManager.getFormats())
    {
        if (format.isNotEmpty() && fmt->getName() != format) continue;
        juce::OwnedArray<juce::PluginDescription> types;
        fmt->findAllTypesForFile (types, path);
        for (auto t : types) found.add (*t);
    }
    // Tracktion only loads external plugins it finds in its list of known plugins.
    for (auto& d : found) engine.getPluginManager().knownPluginList.addType (d);
    return found;
}

juce::AudioBuffer<float> readAudio (const juce::File& file)
{
    juce::AudioFormatManager formats;
    formats.registerBasicFormats();
    juce::AudioBuffer<float> buf;
    if (auto reader = std::unique_ptr<juce::AudioFormatReader> (formats.createReaderFor (file)))
    {
        buf.setSize ((int) reader->numChannels, (int) reader->lengthInSamples);
        reader->read (&buf, 0, buf.getNumSamples(), 0, true, true);
    }
    return buf;
}

/// RMS of the difference of two renders, over what they have in common.
double difference (const juce::AudioBuffer<float>& a, const juce::AudioBuffer<float>& b)
{
    const int n = juce::jmin (a.getNumSamples(), b.getNumSamples());
    const int ch = juce::jmin (a.getNumChannels(), b.getNumChannels());
    if (n == 0 || ch == 0) return -1.0;
    double sum = 0;
    for (int c = 0; c < ch; ++c)
        for (int i = 0; i < n; ++i)
        {
            const double d = (double) a.getSample (c, i) - (double) b.getSample (c, i);
            sum += d * d;
        }
    return std::sqrt (sum / ((double) n * ch));
}

/// Eight seconds of decaying 220 Hz bursts: something an effect visibly changes, and the same
/// every time, which a free-running synth is not.
bool writeTestTone (const juce::File& file)
{
    file.deleteFile();
    const double rate = 44100.0;
    const int n = (int) (rate * 8.0);
    juce::AudioBuffer<float> buf (2, n);
    for (int i = 0; i < n; ++i)
    {
        const double t = i / rate, inBurst = std::fmod (t, 0.5);
        const float v = (float) (0.5 * std::exp (-6.0 * inBurst) * std::sin (2.0 * juce::MathConstants<double>::pi * 220.0 * t));
        buf.setSample (0, i, v);
        buf.setSample (1, i, v);
    }
    juce::WavAudioFormat wav;
    auto stream = file.createOutputStream();
    if (stream == nullptr) return false;
    std::unique_ptr<juce::AudioFormatWriter> writer (wav.createWriterFor (stream.release(), rate, 2, 24, {}, 0));
    return writer != nullptr && writer->writeFromAudioSampleBuffer (buf, 0, n);
}

/// Keeps a plugin's library mapped for the life of the process. Some plugins leave a thread of
/// their own running after their last instance has gone (OB-Xd's timer thread does), and the
/// format unloads the library at that moment, under the thread's feet.
void pinModule (const juce::PluginDescription& desc)
{
   #if JUCE_LINUX
    if (args_noPin) return;
    juce::File bundle (desc.fileOrIdentifier);
    juce::Array<juce::File> libraries;
    if (bundle.isDirectory()) bundle.findChildFiles (libraries, juce::File::findFiles, true, "*.so");
    else if (bundle.hasFileExtension ("so")) libraries.add (bundle);
    for (auto& lib : libraries)
        dlopen (lib.getFullPathName().toRawUTF8(), RTLD_LAZY | RTLD_LOCAL | RTLD_NODELETE);
   #endif
}

/// Puts the plugin on the spike edit's track. An instrument takes FourOsc's place and plays the
/// MIDI clip; an effect gets an audio clip of the test tone to work on instead.
te::ExternalPlugin::Ptr addExternal (te::Edit& edit, const juce::PluginDescription& desc, const juce::File& tone)
{
    auto track = te::getAudioTracks (edit)[0];
    pinModule (desc);
    auto plugin = edit.getPluginCache().createNewPlugin (te::ExternalPlugin::xmlTypeName, desc);
    auto ext = dynamic_cast<te::ExternalPlugin*> (plugin.get());
    if (ext == nullptr) return {};
    if (auto synth = track->pluginList.findFirstPluginOfType<te::FourOscPlugin>())
        synth->deleteFromParent();
    if (! desc.isInstrument)
    {
        for (auto c : track->getClips()) c->removeFromParent();
        const te::ClipPosition pos { { te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (8.0) }, {} };
        if (track->insertWaveClip ("Tone", tone, pos, false) == nullptr) return {};
    }
    track->pluginList.insertPlugin (plugin, 0, nullptr);
    return ext;
}

te::ExternalPlugin* findExternal (te::Edit& edit)
{
    for (auto t : te::getAudioTracks (edit))
        if (auto e = t->pluginList.findFirstPluginOfType<te::ExternalPlugin>())
            return e;
    return nullptr;
}

/// The parameter with this id, or without one the first worth turning: the plugin's own (not
/// the dry and wet levels Tracktion adds), and not a bypass or a MIDI CC mapping.
te::AutomatableParameter::Ptr pickParameter (te::ExternalPlugin& plugin, const juce::String& id)
{
    auto* instance = plugin.getAudioPluginInstance();
    // Stepped parameters snap to their steps and so do not come back as they were set, and a
    // tempo parameter is overwritten by the host's tempo; neither says anything about state.
    auto continuous = [instance] (const juce::String& name)
    {
        if (instance != nullptr)
            for (auto* p : instance->getParameters())
                if (p->getName (256) == name)
                    return ! (p->isDiscrete() || p->isBoolean()) && p->isAutomatable();
        return true;
    };
    for (auto p : plugin.getAutomatableParameters())
    {
        const auto name = p->getParameterName();
        if (p == plugin.dryGain || p == plugin.wetGain) continue;
        if (id.isNotEmpty() ? (p->paramID == id || name == id)
                            : (continuous (name) && ! (name.containsIgnoreCase ("bypass") || name.startsWithIgnoreCase ("midi cc")
                                                       || name.containsIgnoreCase ("beats per minute") || name.containsIgnoreCase ("undefined"))))
            return p;
    }
    return {};
}

//==============================================================================
int cmdScan (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    auto& pm = engine->getPluginManager();
    const auto format = arg (args, "--format", "VST3");
    pm.knownPluginList.clear();
    result ("separate_process", pm.usesSeparateProcessForScanning() ? 1 : 0);
    int found = 0, instruments = 0;
    const auto started = juce::Time::getMillisecondCounterHiRes();
    for (auto fmt : pm.pluginFormatManager.getFormats())
    {
        if (fmt->getName() != format) continue;
        auto paths = fmt->getDefaultLocationsToSearch();
        if (auto extra = arg (args, "--path"); extra.isNotEmpty()) paths = juce::FileSearchPath (extra);
        say ("search path: " + paths.toString());
        auto pedal = juce::File::createTempFile (".pedal");
        juce::PluginDirectoryScanner scanner (pm.knownPluginList, *fmt, paths, true, pedal, false);
        juce::String name;
        while (scanner.scanNextFile (true, name)) {}
        for (auto& f : scanner.getFailedFiles()) say ("failed: " + f);
        result ("failed", scanner.getFailedFiles().size());
        pedal.deleteFile();
    }
    for (auto& d : pm.knownPluginList.getTypes())
    {
        say (d.pluginFormatName + " | " + d.name + " | " + d.manufacturerName + " | " + (d.isInstrument ? "instrument" : "effect")
             + " | in " + juce::String (d.numInputChannels) + " out " + juce::String (d.numOutputChannels) + " | " + d.fileOrIdentifier);
        ++found;
        instruments += d.isInstrument ? 1 : 0;
    }
    result ("scan_ms", juce::Time::getMillisecondCounterHiRes() - started);
    result ("found", found);
    result ("instruments", instruments);
    return found > 0 ? 0 : 1;
}

float pluginSideValue (te::ExternalPlugin& ext, const juce::String& name)
{
    if (auto* instance = ext.getAudioPluginInstance())
        for (auto* p : instance->getParameters())
            if (p->getName (256) == name)
                return p->getValue();
    return -1.0f;
}

int cmdPlugin (const juce::StringArray& args)
{
    args_noPin = args.contains ("--no-pin");
    auto engine = makeEngine (false);
    const auto path = arg (args, "--file");
    auto types = typesForFile (*engine, path, arg (args, "--format"));
    if (types.isEmpty()) { say ("no plugin found in " + path); return 1; }
    juce::PluginDescription desc = types[0];
    if (auto wanted = arg (args, "--name"); wanted.isNotEmpty())
        for (auto& t : types) if (t.name == wanted) desc = t;
    result ("name", desc.name);
    result ("format", desc.pluginFormatName);
    result ("instrument", desc.isInstrument ? 1 : 0);

    auto dir = juce::File::getCurrentWorkingDirectory().getChildFile (arg (args, "--dir", "."));
    auto editFile = dir.getChildFile ("plugin.tracktionedit"), tone = dir.getChildFile ("tone.wav");
    auto wet = dir.getChildFile ("plugin-wet.wav"), wet2 = dir.getChildFile ("plugin-wet2.wav"), dry = dir.getChildFile ("plugin-dry.wav"),
         reloaded = dir.getChildFile ("plugin-reloaded.wav"), automated = dir.getChildFile ("plugin-automated.wav");
    editFile.deleteFile();
    if (! writeTestTone (tone)) { say ("could not write the test tone"); return 1; }

    bool ok = true;
    juce::String paramName, paramId;
    float paramSet = 0;
    double repeatDifference = 0;
    juce::MemoryBlock stateBefore;
    {
        auto edit = makeEdit (*engine, editFile);
        auto ext = addExternal (*edit, desc, tone);
        if (ext == nullptr || ext->getAudioPluginInstance() == nullptr)
        {
            say ("could not load: " + (ext != nullptr ? ext->getLoadError() : juce::String ("not an external plugin")));
            return 1;
        }
        auto* instance = ext->getAudioPluginInstance();
        result ("loaded", 1);
        result ("parameters", ext->getNumAutomatableParameters());
        result ("inputs", ext->getNumInputs());
        result ("outputs", ext->getNumOutputs());
        result ("layout_in", instance->getBusCount (true) > 0 ? instance->getChannelLayoutOfBus (true, 0).getDescription() : juce::String ("none"));
        result ("layout_out", instance->getBusCount (false) > 0 ? instance->getChannelLayoutOfBus (false, 0).getDescription() : juce::String ("none"));
        result ("buses_in", instance->getBusCount (true));
        result ("buses_out", instance->getBusCount (false));
        result ("accepts_midi", instance->acceptsMidi() ? 1 : 0);
        result ("has_editor", instance->hasEditor() ? 1 : 0);
        if (args.contains ("--list-params"))
            for (auto p : ext->getAutomatableParameters())
                say ("param: " + p->paramID + " | " + p->getParameterName() + " | " + juce::String (p->getCurrentNormalisedValue()));

        // Audio: an instrument has to make sound from the clip's notes; an effect has to change
        // the tone that is played into it. Rendering twice tells how repeatable the plugin is,
        // which is what the later differences have to be read against.
        const bool renderedWet = renderEdit (*edit, wet, 9.0);
        renderEdit (*edit, wet2, 9.0);
        const auto wetLevel = measure (wet);
        repeatDifference = difference (readAudio (wet), readAudio (wet2));
        result ("rendered", renderedWet ? 1 : 0);
        result ("peak", wetLevel.peak);
        result ("rms", wetLevel.rms);
        result ("repeat_difference_rms", repeatDifference);
        result ("latency_samples", instance->getLatencySamples());
        result ("latency_seconds", ext->getLatencySeconds());
        ok = ok && renderedWet && wetLevel.peak > 0.001;
        if (! desc.isInstrument)
        {
            ext->setEnabled (false);
            renderEdit (*edit, dry, 9.0);
            ext->setEnabled (true);
            const auto diff = difference (readAudio (wet), readAudio (dry));
            result ("dry_peak", measure (dry).peak);
            result ("dry_vs_tone_rms", difference (readAudio (dry), readAudio (tone)));
            result ("effect_difference_rms", diff);
            ok = ok && diff > 1.0e-4 && diff > repeatDifference * 2.0;
        }

        // A parameter change and the plugin's own state have to survive save and reload. The
        // value only reaches a VST3 plugin's processor with the next block it processes, so a
        // render stands in for the playback that would normally be running.
        if (auto param = pickParameter (*ext, arg (args, "--param")))
        {
            paramName = param->getParameterName();
            paramId = param->paramID;
            const auto before = param->getCurrentNormalisedValue();
            param->setNormalisedParameter (before > 0.5f ? 0.2f : 0.8f, juce::sendNotification);
            renderEdit (*edit, wet2, 1.0);
            pump (200);
            paramSet = param->getCurrentNormalisedValue();
            result ("param", paramName);
            result ("param_before", before);
            result ("param_set", paramSet);
            result ("param_set_plugin_side", pluginSideValue (*ext, paramName));
        }
        ext->flushPluginStateToValueTree();
        instance->getStateInformation (stateBefore);
        const bool saved = te::EditFileOperations (*edit).save (false, true, false);
        result ("saved", saved ? 1 : 0);
        ok = ok && saved;
        pump (300);
    }
    pump (300);

    auto edit = te::loadEditFromFile (*engine, editFile);
    auto ext = edit != nullptr ? findExternal (*edit) : nullptr;
    if (ext == nullptr || ext->getAudioPluginInstance() == nullptr) { say ("the plugin did not come back after reload"); return 1; }
    juce::MemoryBlock stateAfter;
    ext->getAudioPluginInstance()->getStateInformation (stateAfter);
    result ("state_bytes", (int) stateBefore.getSize());
    result ("state_identical", stateBefore == stateAfter ? 1 : 0);
    if (paramName.isNotEmpty())
    {
        auto param = pickParameter (*ext, paramId);
        const auto restored = param != nullptr ? param->getCurrentNormalisedValue() : -1.0f;
        result ("param_restored", restored);
        result ("param_restored_plugin_side", pluginSideValue (*ext, paramName));
        ok = ok && std::abs (restored - paramSet) < 0.01f;

        const bool renderedAgain = renderEdit (*edit, reloaded, 9.0);
        ok = ok && renderedAgain && measure (reloaded).peak > 0.001;
        result ("reloaded_peak", measure (reloaded).peak);

        // Automation: sweep the parameter over the edit and render again.
        if (param != nullptr)
        {
            const auto range = param->getValueRange();
            auto& curve = param->getCurve();
            curve.addPoint (te::TimePosition::fromSeconds (0.0), range.getStart(), 0.0f);
            curve.addPoint (te::TimePosition::fromSeconds (8.0), range.getEnd(), 0.0f);
            result ("automation_points", curve.getNumPoints());
            pump (300);
            const bool renderedAuto = renderEdit (*edit, automated, 9.0);
            result ("automation_rendered", renderedAuto ? 1 : 0);
            result ("automation_difference_rms", difference (readAudio (automated), readAudio (reloaded)));
            ok = ok && renderedAuto && curve.getNumPoints() == 2;
        }
    }
    // Let what the plugin posted to the message thread run while the plugin still exists.
    pump (300);
    edit.reset();
    pump (300);
    if (desc.pluginFormatName == "CLAP")
    {
        result ("clap_activations_off_main_thread", mdaw::clapHostCounters().activationsOffMainThread.load());
        result ("clap_blocks_skipped", mdaw::clapHostCounters().blocksSkipped.load());
    }
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}

/// How many samples `b` lags behind `a`, by the best match within +-512 samples over the first
/// two seconds.
int lagOf (const juce::AudioBuffer<float>& a, const juce::AudioBuffer<float>& b)
{
    const int n = juce::jmin (a.getNumSamples(), b.getNumSamples(), 88200) - 1024;
    if (n <= 0 || a.getNumChannels() == 0 || b.getNumChannels() == 0) return -9999;
    const float* x = a.getReadPointer (0); const float* y = b.getReadPointer (0);
    double best = -1.0e30; int bestLag = 0;
    for (int lag = -512; lag <= 512; ++lag)
    {
        double sum = 0;
        for (int i = 512; i < n; ++i) sum += (double) x[i] * y[i + lag];
        if (sum > best) { best = sum; bestLag = lag; }
    }
    return bestLag;
}

/// Latency compensation. The plugin is set (with --set name=value) to pass its input through,
/// so what is left of it is its delay: measured on a bare instance first, then in a render of
/// its track alone, then next to a second track playing the same clip with no plugin.
int cmdLatency (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    auto types = typesForFile (*engine, arg (args, "--file"), arg (args, "--format"));
    if (types.isEmpty()) { say ("no plugin found"); return 1; }
    const auto desc = types[0];
    pinModule (desc);
    const auto setting = arg (args, "--set");
    const auto setName = setting.upToFirstOccurrenceOf ("=", false, false);
    const auto setValue = setting.fromFirstOccurrenceOf ("=", false, false).getFloatValue();
    auto dir = juce::File::getCurrentWorkingDirectory().getChildFile (arg (args, "--dir", "."));
    auto tone = dir.getChildFile ("tone.wav"), alone = dir.getChildFile ("latency-alone.wav"), both = dir.getChildFile ("latency-both.wav");
    writeTestTone (tone);
    const auto toneAudio = readAudio (tone);

    int rawLag = -9999;
    {
        juce::String error;
        auto raw = engine->getPluginManager().pluginFormatManager.createPluginInstance (desc, 44100.0, 512, error);
        if (raw == nullptr) { say ("could not create a bare instance: " + error); return 1; }
        raw->prepareToPlay (44100.0, 512);
        for (auto* p : raw->getParameters()) if (p->getName (256) == setName) p->setValue (setValue);
        const int channels = juce::jmax (raw->getTotalNumInputChannels(), raw->getTotalNumOutputChannels(), 2);
        const int total = 44100 * 2;
        juce::AudioBuffer<float> out (1, total);
        out.clear();
        for (int pos = 0; pos + 512 <= total; pos += 512)
        {
            juce::AudioBuffer<float> block (channels, 512);
            block.clear();
            block.copyFrom (0, 0, toneAudio, 0, pos, 512);
            block.copyFrom (1, 0, toneAudio, 1, pos, 512);
            juce::MidiBuffer midi;
            raw->processBlock (block, midi);
            out.copyFrom (0, pos, block, 0, 0, 512);
        }
        result ("reported_latency_samples", raw->getLatencySamples());
        rawLag = lagOf (toneAudio, out);
        result ("bare_instance_lag_samples", rawLag);
        raw->releaseResources();
        pump (200);
    }
    pump (200);

    auto edit = makeEdit (*engine, dir.getChildFile ("latency.tracktionedit"));
    auto ext = addExternal (*edit, desc, tone);
    if (ext == nullptr || ext->getAudioPluginInstance() == nullptr) { say ("could not load"); return 1; }
    for (auto p : ext->getAutomatableParameters())
        if (p != ext->dryGain && p != ext->wetGain && p->getParameterName() == setName)
            p->setNormalisedParameter (setValue, juce::sendNotification);
    // A track with no plugin is the reference: Tracktion's own clip playback is not sample-for-
    // sample the file (it came out two samples late and 3 dB down here).
    ext->setEnabled (false);
    renderEdit (*edit, alone, 9.0);
    const int plainLag = lagOf (toneAudio, readAudio (alone));
    ext->setEnabled (true);
    renderEdit (*edit, alone, 9.0);
    const auto aloneAudio = readAudio (alone);
    const int aloneLag = lagOf (toneAudio, aloneAudio);
    result ("plain_track_lag_samples", plainLag);
    result ("track_alone_lag_samples", aloneLag);

    edit->ensureNumberOfAudioTracks (2);
    auto second = te::getAudioTracks (*edit)[1];
    const te::ClipPosition pos { { te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (8.0) }, {} };
    second->insertWaveClip ("Tone", tone, pos, false);
    pump (200);
    renderEdit (*edit, both, 9.0);
    auto bothAudio = readAudio (both);
    // If the plain track were not delayed to match, the two copies would be 'rawLag' apart and
    // the sum would differ from twice the single track by a comb filter.
    auto expected = aloneAudio;
    expected.applyGain (2.0f);
    const auto diff = difference (bothAudio, expected);
    juce::AudioBuffer<float> comb (toneAudio.getNumChannels(), toneAudio.getNumSamples());
    comb.clear();
    for (int c = 0; c < comb.getNumChannels(); ++c)
    {
        comb.addFrom (c, 0, toneAudio, c, 0, toneAudio.getNumSamples());
        if (rawLag > 0) comb.addFrom (c, rawLag, toneAudio, c, 0, toneAudio.getNumSamples() - rawLag);
    }
    auto twice = toneAudio; twice.applyGain (2.0f);
    result ("two_tracks_vs_twice_one_rms", diff);
    result ("uncompensated_would_be_rms", difference (comb, twice));
    result ("two_tracks_lag_samples", lagOf (toneAudio, bothAudio));
    const bool ok = rawLag > 0 && aloneLag == plainLag && diff < 1.0e-4;
    pump (300); edit.reset(); pump (300);
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}

/// What a misbehaving plugin does to a host that runs it in its own process, which is how
/// Tracktion runs every plugin. The repo's CLAP test synth misbehaves on request when it is
/// sent note 0: --mode fail (its processing reports an error), slow (every block takes 50 ms),
/// hang (processing never returns) or crash (it aborts the process).
int cmdMisbehave (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    if (auto error = openDevice (*engine, args); error.isNotEmpty()) { say ("could not open the device: " + error); return 1; }
    auto& adm = engine->getDeviceManager().deviceManager;
    auto* device = adm.getCurrentAudioDevice();
    auto types = typesForFile (*engine, arg (args, "--file"), "CLAP");
    if (types.isEmpty()) { say ("no plugin found"); return 1; }
    const auto mode = arg (args, "--mode", "none");
    const float value = mode == "fail" ? 1.0f : mode == "crash" ? 2.0f : mode == "slow" ? 3.0f : mode == "hang" ? 4.0f : 0.0f;
    result ("mode", mode);

    auto file = juce::File::createTempFile (".tracktionedit");
    auto edit = makeEdit (*engine, file);
    auto ext = addExternal (*edit, types[0], {});
    if (ext == nullptr || ext->getAudioPluginInstance() == nullptr) { say ("could not load"); return 1; }
    // The switch is not automatable, so Tracktion does not list it: set it on the instance.
    bool armed = value == 0.0f;
    for (auto* p : ext->getAudioPluginInstance()->getParameters())
        if (p->getName (64).containsIgnoreCase ("misbehave"))
        {
            p->setValue (value / 4.0f);
            armed = true;
        }
    result ("armed", armed ? 1 : 0);
    if (! armed) { say ("the plugin has no misbehave switch"); return 1; }
    // The trigger: note 0, two seconds in.
    auto track = te::getAudioTracks (*edit)[0];
    dynamic_cast<te::MidiClip*> (track->getClips()[0])->getSequence()
        .addNote (0, te::BeatPosition::fromBeats (4.0), te::BeatDuration::fromBeats (1.0), 100, 0, nullptr);
    // A second track with a built-in synth, to see what becomes of the rest of the project.
    edit->ensureNumberOfAudioTracks (2);
    auto other = te::getAudioTracks (*edit)[1];
    other->pluginList.insertPlugin (edit->getPluginCache().createNewPlugin (te::FourOscPlugin::xmlTypeName, {}), 0, nullptr);
    other->insertMIDIClip ("Drone", { te::TimePosition::fromSeconds (0.0), te::TimePosition::fromSeconds (8.0) }, nullptr)
        ->getSequence().addNote (45, te::BeatPosition::fromBeats (0.0), te::BeatDuration::fromBeats (16.0), 100, 0, nullptr);

    // If the message thread gets stuck behind the audio thread, say so and leave.
    std::atomic<int> stage { 0 };
    std::thread watchdog ([&stage]
    {
        for (int i = 0; i < 150 && stage.load() < 3; ++i) juce::Thread::sleep (100);
        if (stage.load() < 3)
        {
            std::cout << "RESULT message_thread_stuck_at_stage=" << stage.load() << std::endl << "RESULT ok=0" << std::endl;
            std::_Exit (3);
        }
    });

    auto& transport = edit->getTransport();
    const int xrunsBefore = device->getXRunCount();
    transport.ensureContextAllocated();
    transport.setPosition (te::TimePosition::fromSeconds (0.0));
    transport.play (false);
    const auto before = keepPlaying (*edit, adm, 1700);
    result ("position_before_trigger", transport.getPosition().inSeconds());
    result ("peak_before_trigger", before.peak);
    pump (800);   // the trigger is at 2.0 s
    stage = 1;
    auto level = adm.getOutputLevelGetter();
    const double posA = transport.getPosition().inSeconds();
    double peakAfter = 0, cpu = 0;
    for (int i = 0; i < 100; ++i) { pump (20); peakAfter = juce::jmax (peakAfter, level->getCurrentLevel()); cpu = juce::jmax (cpu, adm.getCpuUsage()); }
    const double posB = transport.getPosition().inSeconds();
    result ("position_after_trigger", posA);
    result ("position_2s_later", posB);
    result ("position_advanced_seconds", posB - posA);
    result ("still_playing", transport.isPlaying() ? 1 : 0);
    result ("peak_after_trigger", peakAfter);
    result ("max_cpu_after_trigger", cpu);
    result ("xruns", device->getXRunCount() - xrunsBefore);
    result ("clap_blocks_skipped", mdaw::clapHostCounters().blocksSkipped.load());
    stage = 2;
    transport.stop (false, false);
    pump (300);
    result ("stop_returned", 1);
    stage = 3;
    watchdog.join();
    if (mode == "hang") { result ("ok", 1); std::cout.flush(); std::_Exit (0); }   // tearing down would wait for the audio thread
    pump (300); edit.reset(); pump (300);
    file.deleteFile();
    result ("ok", 1);
    return 0;
}

/// Opens the plugin's own editor in a native window next to nothing at all, twice, and checks
/// that a change made the way an editor makes it reaches Tracktion's parameter.
int cmdEditor (const juce::StringArray& args)
{
    auto engine = makeEngine (false);
    auto types = typesForFile (*engine, arg (args, "--file"), arg (args, "--format"));
    if (types.isEmpty()) { say ("no plugin found"); return 1; }
    auto file = juce::File::createTempFile (".tracktionedit");
    auto edit = makeEdit (*engine, file);
    auto tone = juce::File::createTempFile (".wav");
    writeTestTone (tone);
    auto ext = addExternal (*edit, types[0], tone);
    auto* instance = ext != nullptr ? ext->getAudioPluginInstance() : nullptr;
    if (instance == nullptr) { say ("could not load"); return 1; }
    result ("has_editor", instance->hasEditor() ? 1 : 0);
    if (! instance->hasEditor()) return 1;
    edit->getTransport().ensureContextAllocated();

    struct Window : juce::DocumentWindow
    {
        Window (const juce::String& name, juce::AudioProcessorEditor* editor)
            : DocumentWindow (name, juce::Colours::black, DocumentWindow::closeButton)
        {
            setUsingNativeTitleBar (true);
            setContentOwned (editor, true);
            setVisible (true);
        }
        void closeButtonPressed() override { closed = true; }
        bool closed = false;
    };

    bool ok = true;
    const int holdMs = arg (args, "--hold", "1500").getIntValue();
    for (int round = 0; round < 2; ++round)
    {
        auto* editor = instance->createEditorIfNeeded();
        if (editor == nullptr) { say ("the plugin made no editor"); return 1; }
        auto window = std::make_unique<Window> (types[0].name, editor);
        // While the window is up, a person may turn knobs: report what the engine saw change.
        std::vector<float> before;
        for (auto p : ext->getAutomatableParameters()) before.push_back (p->getCurrentNormalisedValue());
        for (int waited = 0; waited < holdMs && ! window->closed; waited += 50) pump (50);
        result ("round" + juce::String (round) + "_closed_by_hand", window->closed ? 1 : 0);
        int turned = 0;
        for (int i = 0; i < ext->getNumAutomatableParameters(); ++i)
        {
            auto p = ext->getAutomatableParameters()[i];
            if (std::abs (p->getCurrentNormalisedValue() - before[(size_t) i]) > 1.0e-4f && ++turned <= 8)
                say ("TURNED " + p->getParameterName() + ": " + juce::String (before[(size_t) i], 3) + " -> " + juce::String (p->getCurrentNormalisedValue(), 3));
        }
        result ("round" + juce::String (round) + "_parameters_turned_by_hand", turned);
        const bool onScreen = window->isOnDesktop() && window->getPeer() != nullptr && window->isShowing();
        result ("round" + juce::String (round) + "_on_screen", onScreen ? 1 : 0);
        result ("round" + juce::String (round) + "_size", juce::String (editor->getWidth()) + "x" + juce::String (editor->getHeight()));
        ok = ok && onScreen && editor->getWidth() > 0;

        if (round == 0)
            if (auto param = pickParameter (*ext, arg (args, "--param")))
            {
                // What a plugin's editor does when a knob is turned: tell the host.
                const auto index = ext->getAutomatableParameters().indexOf (param.get());
                juce::ignoreUnused (index);
                for (auto* p : instance->getParameters())
                    if (p->getName (256) == param->getParameterName())
                    {
                        const float target = p->getValue() > 0.5f ? 0.25f : 0.75f;
                        p->beginChangeGesture();
                        p->setValueNotifyingHost (target);
                        p->endChangeGesture();
                        pump (300);
                        result ("ui_change_param", param->getParameterName());
                        result ("ui_change_target", target);
                        result ("ui_change_seen_by_engine", param->getCurrentNormalisedValue());
                        ok = ok && std::abs (param->getCurrentNormalisedValue() - target) < 0.02f;
                        break;
                    }
            }
        window.reset();
        pump (300);
        result ("round" + juce::String (round) + "_closed", instance->getActiveEditor() == nullptr ? 1 : 0);
        ok = ok && instance->getActiveEditor() == nullptr;
    }
    pump (300);
    edit.reset();
    pump (300);
    file.deleteFile();
    if (types[0].pluginFormatName == "CLAP")
    {
        result ("clap_editors_shown", mdaw::clapHostCounters().editorsShown.load());
        ok = ok && mdaw::clapHostCounters().editorsShown.load() == 2;
    }
    result ("ok", ok ? 1 : 0);
    return ok ? 0 : 1;
}

int pluginCommands (const juce::String& cmd, const juce::StringArray& args)
{
    if (cmd == "scan")   return cmdScan (args);
    if (cmd == "latency") return cmdLatency (args);
    if (cmd == "misbehave") return cmdMisbehave (args);
    if (cmd == "plugin") return cmdPlugin (args);
    if (cmd == "editor") return cmdEditor (args);
    return -1;
}
} // namespace
