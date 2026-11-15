#include "plugin_ids.h"
#include "clap_format.h"

#include <array>
#include <cstring>
#include <functional>

#if JUCE_LINUX || JUCE_BSD
 #include <dlfcn.h>
 #include <sys/utsname.h>
#elif JUCE_MAC
 #include <CoreFoundation/CoreFoundation.h>
#elif JUCE_WINDOWS
 #ifndef NOMINMAX
  #define NOMINMAX
 #endif
 #include <windows.h>
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

juce::String PluginIds::of (const juce::PluginDescription& d, bool mayLoad)
{
    if (d.pluginFormatName == "LV2")
        return d.fileOrIdentifier;   // the plugin URI
    if (d.pluginFormatName != "CLAP" && d.pluginFormatName != "VST3")
        return {};

    const auto key = keyOf (d);
    if (auto found = ids.find (key); found != ids.end())
        return found->second;
    if (! mayLoad)
        return {};
    const auto id = ownIdOf (d);
    ids[key] = id;
    dirty = dirty || id.isNotEmpty();
    return id;
}

void PluginIds::remember (const juce::PluginDescription& d, const juce::String& id)
{
    if (d.pluginFormatName == "LV2" || id.isEmpty())
        return;
    auto& slot = ids[keyOf (d)];
    dirty = dirty || slot != id;
    slot = id;
}

bool PluginIds::knows (const juce::PluginDescription& d) const
{
    if (d.pluginFormatName == "LV2")
        return true;
    auto found = ids.find (keyOf (d));
    return found != ids.end() && found->second.isNotEmpty();
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

juce::String ownIdOf (const juce::PluginDescription& d)
{
    if (d.pluginFormatName == "LV2")  return d.fileOrIdentifier;
    if (d.pluginFormatName == "CLAP") return mdaw::clapPluginId (d);
    if (d.pluginFormatName == "VST3") return vst3ClassId (d);
    return {};
}

//==============================================================================
juce::String canonicalVst3Id (const std::uint8_t tuid[16], bool comLayout)
{
    // INLINE_UID with COM_COMPATIBLE (pluginterfaces/base/funknown.h) stores l1 as 4 bytes
    // least significant first, then the high and the low 16 bits of l2 each least significant
    // first; bytes 8 to 15 (l3, l4) are most significant first in both layouts.
    static constexpr std::array<int, 16> comOrder { 3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15 };
    std::uint8_t bytes[16];
    for (size_t i = 0; i < 16; ++i)
        bytes[i] = tuid[comLayout ? (size_t) comOrder[i] : i];
    return juce::String::toHexString (bytes, 16, 0).toUpperCase();
}

int juceUniqueIdOfVst3Id (const juce::String& id)
{
    if (id.length() != 32 || ! id.containsOnly ("0123456789abcdefABCDEF"))
        return 0;
    std::uint32_t value = 0;
    for (int l = 0; l < 4; ++l)
        value = value * 31 + (std::uint32_t) id.substring (l * 8, l * 8 + 8).getHexValue32();
    return (int) value;
}

std::vector<juce::String> vst3ClassIdsFromModuleInfo (const juce::File& bundle)
{
    // Where JUCE's VST3 format looks too (DescriptionLister::findDescriptionsFast): the SDK's
    // place first, then the older one.
    std::vector<juce::String> result;
    for (auto& info : { bundle.getChildFile ("Contents/Resources/moduleinfo.json"), bundle.getChildFile ("Contents/moduleinfo.json") })
    {
        if (! info.existsAsFile())
            continue;
        // moduleinfo.json is JSON5 by the SDK's definition; one that juce::JSON cannot read
        // (comments, trailing commas) makes the caller fall back to the factory.
        const auto json = juce::JSON::parse (info.loadFileAsString());
        if (auto* classes = json["Classes"].getArray())
            for (auto& c : *classes)
                if (c["Category"].toString() == "Audio Module Class")
                    if (auto cid = c["CID"].toString().toUpperCase(); cid.length() == 32 && cid.containsOnly ("0123456789ABCDEF"))
                        result.push_back (cid);
        if (! result.empty())
            break;
    }
    return result;
}

//==============================================================================
// The VST3 factory, called through its vtable: the VST3 SDK's interfaces are plain C++ classes
// with only pure virtual functions, so their layout is fixed (FUnknown: queryInterface, addRef,
// release; IPluginFactory: getFactoryInfo, countClasses, getClassInfo, createInstance). On
// Windows the SDK declares them __stdcall, which is the one calling convention there on x64;
// 32-bit Windows is not supported here.
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

using FactoryProc = IPluginFactory* (*)();

#if JUCE_WINDOWS
constexpr bool tuidInComLayout = true;    // COM_COMPATIBLE is 1 on Windows only
#else
constexpr bool tuidInComLayout = false;
#endif

/// Loads the module of a VST3 bundle (or single-file plugin), calls its entry function, hands
/// its factory to `use`, then calls its exit function. The module stays loaded for the life of
/// the process, like every plugin library (doc point 6). Linux is tested; the macOS and
/// Windows branches are compiled for those platforms but have NOT been run on them.
template <typename Use>
void withVst3Factory (const juce::File& bundle, Use&& use)
{
    FactoryProc getFactory = nullptr;
    std::function<bool()> enter, leave;

   #if JUCE_LINUX || JUCE_BSD
    // Contents/<machine>-linux/<bundle name>.so, where the SDK's module loader looks.
    juce::File library (bundle);
    if (bundle.isDirectory())
    {
        struct utsname names {};
        if (uname (&names) != 0)
            return;
        library = bundle.getChildFile ("Contents").getChildFile (juce::String (names.machine) + "-linux")
                        .getChildFile (bundle.getFileNameWithoutExtension() + ".so");
    }
    if (! library.existsAsFile())
        return;
    void* handle = dlopen (library.getFullPathName().toRawUTF8(), RTLD_LAZY | RTLD_LOCAL | RTLD_NODELETE);
    if (handle == nullptr)
        return;
    getFactory = reinterpret_cast<FactoryProc> (dlsym (handle, "GetPluginFactory"));
    if (auto entry = reinterpret_cast<bool (*) (void*)> (dlsym (handle, "ModuleEntry")))
        enter = [entry, handle] { return entry (handle); };
    if (auto exit = reinterpret_cast<bool (*)()> (dlsym (handle, "ModuleExit")))
        leave = [exit] { return exit(); };
   #elif JUCE_MAC
    // A bundle: CoreFoundation finds its executable (Contents/MacOS/<name>) from Info.plist.
    // The bundle object is never released, so the executable stays loaded.
    const auto path = bundle.getFullPathName();
    CFURLRef url = CFURLCreateFromFileSystemRepresentation (nullptr, (const UInt8*) path.toRawUTF8(),
                                                            (CFIndex) path.getNumBytesAsUTF8(), bundle.isDirectory());
    CFBundleRef cfBundle = url != nullptr ? CFBundleCreate (kCFAllocatorDefault, url) : nullptr;
    if (url != nullptr)
        CFRelease (url);
    if (cfBundle == nullptr || ! CFBundleLoadExecutable (cfBundle))
        return;
    getFactory = reinterpret_cast<FactoryProc> (CFBundleGetFunctionPointerForName (cfBundle, CFSTR ("GetPluginFactory")));
    if (auto entry = reinterpret_cast<bool (*) (CFBundleRef)> (CFBundleGetFunctionPointerForName (cfBundle, CFSTR ("bundleEntry"))))
        enter = [entry, cfBundle] { return entry (cfBundle); };
    if (auto exit = reinterpret_cast<bool (*)()> (CFBundleGetFunctionPointerForName (cfBundle, CFSTR ("bundleExit"))))
        leave = [exit] { return exit(); };
   #elif JUCE_WINDOWS
    // A bundle (Contents/<arch>-win/<name>.vst3) or the older single-file .vst3 DLL. InitDll
    // and ExitDll are optional. The module is never freed.
    juce::File library (bundle);
    if (bundle.isDirectory())
    {
       #if defined (_M_ARM64) || defined (__aarch64__)
        const char* arch = "arm64-win";
       #else
        const char* arch = "x86_64-win";
       #endif
        library = bundle.getChildFile ("Contents").getChildFile (arch).getChildFile (bundle.getFileName());
    }
    if (! library.existsAsFile())
        return;
    HMODULE module = LoadLibraryW (library.getFullPathName().toWideCharPointer());
    if (module == nullptr)
        return;
    getFactory = reinterpret_cast<FactoryProc> (GetProcAddress (module, "GetPluginFactory"));
    if (auto entry = reinterpret_cast<bool (*)()> (GetProcAddress (module, "InitDll")))
        enter = [entry] { return entry(); };
    if (auto exit = reinterpret_cast<bool (*)()> (GetProcAddress (module, "ExitDll")))
        leave = [exit] { return exit(); };
   #else
    juce::ignoreUnused (bundle, use);
    return;
   #endif

    if (getFactory == nullptr || (enter != nullptr && ! enter()))
        return;
    if (auto* factory = getFactory())
    {
        use (factory);
        factory->vtbl->release (factory);
    }
    if (enter != nullptr && leave != nullptr)
        leave();
}
} // namespace

std::vector<juce::String> vst3ClassIdsFromFactory (const juce::File& bundle)
{
    std::vector<juce::String> result;
    withVst3Factory (bundle, [&] (IPluginFactory* factory)
    {
        const auto count = factory->vtbl->countClasses (factory);
        for (int32_t i = 0; i < count; ++i)
        {
            PClassInfo info {};
            if (factory->vtbl->getClassInfo (factory, i, &info) != 0)   // kResultOk is 0
                continue;
            if (std::strncmp (info.category, "Audio Module Class", sizeof (info.category)) == 0)
                result.push_back (canonicalVst3Id (info.cid, tuidInComLayout));
        }
    });
    return result;
}

juce::String vst3ClassId (const juce::PluginDescription& d)
{
    if (d.pluginFormatName != "VST3")
        return {};
    const juce::File bundle (d.fileOrIdentifier);
    // JUCE names the class by a hash of it (uniqueId); a bundle may hold several classes.
    for (auto& id : vst3ClassIdsFromModuleInfo (bundle))
        if (juceUniqueIdOfVst3Id (id) == d.uniqueId)
            return id;
    for (auto& id : vst3ClassIdsFromFactory (bundle))
        if (juceUniqueIdOfVst3Id (id) == d.uniqueId)
            return id;
    return {};
}
} // namespace mdaw::bridge
