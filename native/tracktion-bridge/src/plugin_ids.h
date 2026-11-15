// A plugin's OWN id, the one mdaw_plugins_list reports and mdaw_device_add takes: the same on
// every machine that has the plugin, wherever its files are. JUCE's identifier string carries a
// hash of the file's path, so it cannot be used in a shared project.
//
//   CLAP  the CLAP plugin id ("org.example.synth"), read from the plugin's library
//   LV2   the plugin URI (JUCE's LV2 format keeps it as the description's fileOrIdentifier)
//   VST3  the component's class id (TUID) as 32 upper-case hex digits in the canonical order:
//         the FUID's four 32-bit words, most significant byte first. That is what the VST3
//         SDK's FUID::toString prints on every platform and what moduleinfo.json's "CID"
//         holds. Taken from Contents/Resources/moduleinfo.json when the bundle has one (no
//         library is loaded), otherwise from the plugin's factory, since JUCE's description
//         keeps only a 32-bit hash of it.
//
// Reading CLAP ids, and VST3 ids without moduleinfo.json, loads the plugin's library (not an
// instance), so the ids are kept in a file in the settings folder, keyed by the description's
// file, its modification time and JUCE's uniqueId: a restart lists the remembered plugins
// without loading any library. With a scanner (mdaw-plugin-scan) the ids are read in the
// scanner's process and handed to remember(); this process then never reads one itself.
#pragma once

#include <juce_audio_processors/juce_audio_processors.h>

#include <cstdint>
#include <map>
#include <vector>

namespace mdaw::bridge
{
class PluginIds
{
public:
    explicit PluginIds (juce::File cacheFile);

    /// The plugin's own id; empty if it cannot be had (not a VST3, LV2 or CLAP description, or
    /// the library no longer has the plugin the description names). With mayLoad false only a
    /// remembered id is given (or an LV2 URI): no library is ever loaded.
    juce::String of (const juce::PluginDescription&, bool mayLoad = true);

    /// Takes an id read elsewhere (by the scanner) for this description.
    void remember (const juce::PluginDescription&, const juce::String& id);

    /// Whether an id is remembered for this description (LV2 needs none).
    bool knows (const juce::PluginDescription&) const;

    /// Writes the remembered ids if any were added.
    void save();

private:
    static juce::String keyOf (const juce::PluginDescription&);
    juce::File file;
    std::map<juce::String, juce::String> ids;   // key -> id; "" for "cannot be had" (not saved)
    bool dirty = false;
};

/// The plugin's own id read from the plugin itself: may load its library (CLAP; VST3 without
/// moduleinfo.json). What PluginIds::of uses when it has nothing remembered, and what the
/// scanner uses.
juce::String ownIdOf (const juce::PluginDescription&);

/// The class id of the VST3 component the description names: from moduleinfo.json if the
/// bundle has one, otherwise from the plugin's factory.
juce::String vst3ClassId (const juce::PluginDescription&);

/// The canonical ids of the "Audio Module Class" classes of a VST3 bundle or file, from
/// moduleinfo.json only (empty if it has none or it does not parse) or from the factory only
/// (loads the library). Apart so that the two can be compared.
std::vector<juce::String> vst3ClassIdsFromModuleInfo (const juce::File& bundle);
std::vector<juce::String> vst3ClassIdsFromFactory (const juce::File& bundle);

/// A TUID as the canonical 32 hex digits. `comLayout`: the TUID is in the COM-compatible
/// layout the VST3 SDK uses on Windows (INLINE_UID with COM_COMPATIBLE), where the first eight
/// bytes are a GUID's Data1 (32 bit), Data2 and Data3 (16 bit) in little-endian order;
/// otherwise its bytes are already in canonical order (Linux, macOS).
juce::String canonicalVst3Id (const std::uint8_t tuid[16], bool comLayout);

/// JUCE's uniqueId (VST3PluginFormat's getHashForRange over getNormalisedTUID) of a
/// canonical id; 0 for text that is not 32 hex digits.
int juceUniqueIdOfVst3Id (const juce::String& canonical);
} // namespace mdaw::bridge
