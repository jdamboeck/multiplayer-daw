#include "plugin_ids.h"
#include "clap_format.h"

#include <array>
#include <cstring>

#if JUCE_LINUX
 #include <dlfcn.h>
 #include <sys/utsname.h>
#endif

namespace mdaw::bridge
{
PluginIds::PluginIds (juce::File cacheFile) : file (std::move (cacheFile))
{
    // Entries are only ever looked up by an exact key, so a stale or foreign entry is never
    // used: at worst it takes a little room until the file is next written.
    if (auto xml = juce::XmlDocument::parse (file))
        for (auto* e : xml->getChildWithTagNameIterator ("ID"))
            if (auto key = e->getStringAttribute ("key"), id = e->getStringAttribute ("id"); key.isNotEmpty() && id.isNotEmpty())
                ids[key] = id;
}

juce::String PluginIds::keyOf (const juce::PluginDescription& d)
{
    return d.pluginFormatName + "|" + d.fileOrIdentifier + "|" + juce::String::toHexString (d.uniqueId)
           + "|" + juce::String::toHexString (d.lastFileModTime.toMilliseconds());
}

juce::String PluginIds::of (const juce::PluginDescription& d)
{
    if (d.pluginFormatName == "LV2")
        return d.fileOrIdentifier;   // the plugin URI
    if (d.pluginFormatName != "CLAP" && d.pluginFormatName != "VST3")
        return {};

    const auto key = keyOf (d);
    if (auto found = ids.find (key); found != ids.end())
        return found->second;
    const auto id = d.pluginFormatName == "CLAP" ? mdaw::clapPluginId (d) : vst3ClassId (d);
    ids[key] = id;
    dirty = dirty || id.isNotEmpty();
    return id;
}

void PluginIds::save()
{
    if (! std::exchange (dirty, false))
        return;
    juce::XmlElement root ("PLUGIN_IDS");
    for (auto& [key, id] : ids)
        if (id.isNotEmpty())
        {
            auto* e = root.createNewChildElement ("ID");
            e->setAttribute ("key", key);
            e->setAttribute ("id", id);
        }
    root.writeTo (file);
}

//==============================================================================
// The VST3 factory, called through its vtable: the VST3 SDK's interfaces are plain C++ classes
// with only pure virtual functions, so their layout is fixed (FUnknown: queryInterface, addRef,
// release; IPluginFactory: getFactoryInfo, countClasses, getClassInfo, createInstance).
namespace
{
struct PClassInfo
{
    unsigned char cid[16];
    int32_t cardinality;
    char category[32];
    char name[64];
};

struct IPluginFactory
{
    struct Vtbl
    {
        int32_t (*queryInterface) (IPluginFactory*, const char*, void**);
        uint32_t (*addRef) (IPluginFactory*);
        uint32_t (*release) (IPluginFactory*);
        int32_t (*getFactoryInfo) (IPluginFactory*, void*);
        int32_t (*countClasses) (IPluginFactory*);
        int32_t (*getClassInfo) (IPluginFactory*, int32_t, PClassInfo*);
    };
    const Vtbl* vtbl;
};

/// JUCE's getHashForRange over getNormalisedTUID: how it computes the description's uniqueId.
int juceUniqueIdOf (const std::array<uint32_t, 4>& longs)
{
    uint32_t value = 0;
    for (auto item : longs)
        value = value * 31 + item;
    return (int) value;
}
} // namespace

juce::String vst3ClassId (const juce::PluginDescription& d)
{
   #if JUCE_LINUX
    if (d.pluginFormatName != "VST3")
        return {};
    juce::File library (d.fileOrIdentifier);
    if (library.isDirectory())
    {
        // Where JUCE (and the VST3 SDK) look: Contents/<machine>-linux/<bundle name>.so.
        struct utsname names {};
        if (uname (&names) != 0)
            return {};
        library = library.getChildFile ("Contents").getChildFile (juce::String (names.machine) + "-linux")
                         .getChildFile (library.getFileNameWithoutExtension() + ".so");
    }
    if (! library.existsAsFile())
        return {};

    // Kept mapped for the life of the process, like every plugin library (doc point 6).
    void* handle = dlopen (library.getFullPathName().toRawUTF8(), RTLD_LAZY | RTLD_LOCAL | RTLD_NODELETE);
    if (handle == nullptr)
        return {};
    using EntryProc = bool (*) (void*);
    using ExitProc = bool (*)();
    using FactoryProc = IPluginFactory* (*)();
    auto entry = reinterpret_cast<EntryProc> (dlsym (handle, "ModuleEntry"));
    auto leave = reinterpret_cast<ExitProc> (dlsym (handle, "ModuleExit"));
    auto getFactory = reinterpret_cast<FactoryProc> (dlsym (handle, "GetPluginFactory"));
    juce::String result;
    if (getFactory != nullptr && (entry == nullptr || entry (handle)))
    {
        if (auto* factory = getFactory())
        {
            const auto count = factory->vtbl->countClasses (factory);
            for (int32_t i = 0; i < count && result.isEmpty(); ++i)
            {
                PClassInfo info {};
                if (factory->vtbl->getClassInfo (factory, i, &info) != 0)   // kResultOk is 0
                    continue;
                if (std::strncmp (info.category, "Audio Module Class", sizeof (info.category)) != 0)
                    continue;
                // On Linux (no COM byte order) the TUID's bytes are the FUID's four longs, most
                // significant byte first: the bytes in order are the normalised id.
                std::array<uint32_t, 4> longs {};
                for (size_t l = 0; l < 4; ++l)
                    longs[l] = (uint32_t) info.cid[l * 4] << 24 | (uint32_t) info.cid[l * 4 + 1] << 16
                             | (uint32_t) info.cid[l * 4 + 2] << 8 | (uint32_t) info.cid[l * 4 + 3];
                if (juceUniqueIdOf (longs) != d.uniqueId)
                    continue;
                result = juce::String::toHexString (info.cid, 16, 0).toUpperCase();
            }
            factory->vtbl->release (factory);
        }
        if (entry != nullptr && leave != nullptr)
            leave();
    }
    return result;
   #else
    juce::ignoreUnused (d);
    return {};
   #endif
}
} // namespace mdaw::bridge
