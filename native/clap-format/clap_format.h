// A CLAP host behind JUCE's plugin format interface.
//
// Registered with `engine.getPluginManager().pluginFormatManager.addFormat (...)`, it makes a
// CLAP plugin an ordinary `juce::AudioPluginInstance`: Tracktion's ExternalPlugin then gives it
// the same parameters, automation, state, latency compensation and editor window handling as
// a VST3 plugin, with no CLAP-specific code anywhere else.
//
// Threads: everything except processBlock() and reset() is called on the message thread, which
// is CLAP's main thread. Plugin libraries are never unloaded.
#pragma once

#include <juce_audio_processors/juce_audio_processors.h>

namespace mdaw
{
    std::unique_ptr<juce::AudioPluginFormat> createClapFormat();

    /// The CLAP plugin id ("org.example.synth") of a plugin this format described, read from
    /// the plugin's library (loaded once and kept, as for an instance). Empty if the description
    /// is not a CLAP one or the library no longer has that plugin.
    juce::String clapPluginId (const juce::PluginDescription&);

    /// The plugin's own plain range of a parameter of a CLAP instance made by this format.
    /// False, leaving min and max as they are, for any other parameter.
    bool clapParameterRange (const juce::AudioProcessorParameter*, double& min, double& max);

    /// Things the host did that CLAP does not allow, counted so that a test can see them.
    struct ClapHostCounters
    {
        std::atomic<int> activationsOffMainThread { 0 };
        std::atomic<int> blocksSkipped { 0 };
        /// Editors whose plugin accepted the host's window as its parent.
        std::atomic<int> editorsShown { 0 };
    };
    ClapHostCounters& clapHostCounters();
}
