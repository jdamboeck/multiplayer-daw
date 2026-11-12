// A plugin's OWN id, the one mdaw_plugins_list reports and mdaw_device_add takes: the same on
// every machine that has the plugin, wherever its files are. JUCE's identifier string carries a
// hash of the file's path, so it cannot be used in a shared project.
//
//   CLAP  the CLAP plugin id ("org.example.synth"), read from the plugin's library
//   LV2   the plugin URI (JUCE's LV2 format keeps it as the description's fileOrIdentifier)
//   VST3  the component's class id (TUID) as 32 upper-case hex digits, in the byte order of the
//         VST3 SDK's FUID::toString on Linux and of moduleinfo.json; read from the plugin's
//         factory, since JUCE's description keeps only a 32-bit hash of it
//
// Reading CLAP and VST3 ids loads the plugin's library (not an instance), so the ids are kept
// in a file in the settings folder, keyed by the description's file, its modification time and
// JUCE's uniqueId: a restart lists the remembered plugins without loading any library.
#pragma once

#include <juce_audio_processors/juce_audio_processors.h>

#include <map>

namespace mdaw::bridge
{
class PluginIds
{
public:
    explicit PluginIds (juce::File cacheFile);

    /// The plugin's own id; empty if it cannot be had (not a VST3, LV2 or CLAP description, or
    /// the library no longer has the plugin the description names).
    juce::String of (const juce::PluginDescription&);

    /// Writes the remembered ids if any were added.
    void save();

private:
    static juce::String keyOf (const juce::PluginDescription&);
    juce::File file;
    std::map<juce::String, juce::String> ids;   // key -> id; "" for "cannot be had" (not saved)
    bool dirty = false;
};

/// The class id of the VST3 component the description names, read from the plugin's factory.
juce::String vst3ClassId (const juce::PluginDescription&);
} // namespace mdaw::bridge
