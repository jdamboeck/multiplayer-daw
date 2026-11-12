// The child side of the plugin scan; see scanner.h for the protocol.
#include "scanner.h"
#include "clap_format.h"
#include "plugin_ids.h"
#include "x11_atoms.h"

#include <juce_audio_processors/juce_audio_processors.h>

#include <cstdio>
#include <cstdlib>

#if JUCE_LINUX || JUCE_MAC || JUCE_BSD
 #include <unistd.h>
#endif

namespace mdaw::bridge
{
namespace
{
int fail (int status, const juce::String& message)
{
    std::fprintf (stderr, "mdaw-plugin-scan: %s\n", message.toRawUTF8());
    std::fflush (stderr);
    return status;
}

/// Only the format asked for is made: the LV2 format reads every bundle's data when it is
/// created, the others cost nothing.
std::unique_ptr<juce::AudioPluginFormat> formatNamed (const juce::String& name)
{
   #if JUCE_PLUGINHOST_VST3
    if (name == "VST3") return std::make_unique<juce::VST3PluginFormat>();
   #endif
   #if JUCE_PLUGINHOST_LV2
    if (name == "LV2") return std::make_unique<juce::LV2PluginFormat>();
   #endif
    if (name == "CLAP") return mdaw::createClapFormat();
    return {};
}

bool writeAll (int fd, const char* data, size_t size)
{
   #if JUCE_LINUX || JUCE_MAC || JUCE_BSD
    while (size > 0)
    {
        const auto n = ::write (fd, data, size);
        if (n <= 0)
            return false;
        data += n;
        size -= (size_t) n;
    }
    return true;
   #else
    juce::ignoreUnused (fd);
    return std::fwrite (data, 1, size, stdout) == size && std::fflush (stdout) == 0;
   #endif
}

int examine (const juce::String& formatName, const juce::String& file, const juce::String& resultPath, int resultFd)
{
    auto format = formatNamed (formatName);
    if (format == nullptr)
        return fail (2, "no plugin format '" + formatName + "'");

    juce::OwnedArray<juce::PluginDescription> found;
    format->findAllTypesForFile (found, file);

    juce::XmlElement root ("MDAW_PLUGIN_SCAN");
    root.setAttribute ("version", 1);
    root.setAttribute ("format", formatName);
    root.setAttribute ("file", file);
    for (auto* d : found)
    {
        auto* entry = root.createNewChildElement ("FOUND");
        // The own id while the library is loaded anyway; empty if it cannot be had.
        entry->setAttribute ("id", ownIdOf (*d));
        entry->addChildElement (d->createXml().release());
    }
    const auto text = root.toString();

    if (resultPath.isNotEmpty())
    {
        const juce::File result (resultPath);
        const auto part = result.getSiblingFile (result.getFileName() + ".part");
        if (! part.replaceWithText (text) || ! part.moveFileTo (result))
            return fail (5, "cannot write " + result.getFullPathName());
        return 0;
    }
    if (! writeAll (resultFd, text.toRawUTF8(), text.getNumBytesAsUTF8()))
        return fail (5, "cannot write the result to stdout");
    return 0;
}

int selfTest (const juce::StringArray& bundles)
{
    int failures = vst3IdByteOrderCheck();
    for (auto& path : bundles)
    {
        const juce::File bundle (path);
        const auto fromInfo = vst3ClassIdsFromModuleInfo (bundle);
        const auto fromFactory = vst3ClassIdsFromFactory (bundle);
        juce::StringArray a, b;
        for (auto& id : fromInfo) a.add (id);
        for (auto& id : fromFactory) b.add (id);
        a.sort (false);
        b.sort (false);
        const bool ok = ! b.isEmpty() && (a.isEmpty() || a == b);
        std::printf ("%s %s: moduleinfo [%s] factory [%s]\n", ok ? "ok  " : "FAIL", path.toRawUTF8(),
                     a.joinIntoString (" ").toRawUTF8(), b.joinIntoString (" ").toRawUTF8());
        failures += ok ? 0 : 1;
    }
    std::printf ("self-test failures: %d\n", failures);
    std::fflush (stdout);
    return failures == 0 ? 0 : 1;
}
} // namespace

int pluginScanMain (int argc, char** argv)
{
    // Set the real stdout aside and send whatever the plugins print there to stderr.
    int resultFd = 1;
   #if JUCE_LINUX || JUCE_MAC || JUCE_BSD
    resultFd = ::dup (1);
    if (resultFd < 0 || ::dup2 (2, 1) < 0)
        return fail (5, "cannot set stdout aside");
   #endif

    juce::StringArray args;
    for (int i = 1; i < argc; ++i)
        args.add (juce::String::fromUTF8 (argv[i]));

    if (args[0] == "--self-test")
    {
        // Results to the real stdout, as the self-test is for people.
       #if JUCE_LINUX || JUCE_MAC || JUCE_BSD
        ::dup2 (resultFd, 1);
       #endif
        mdaw::defineWindowManagerAtoms();
        const juce::ScopedJuceInitialiser_GUI juceInitialised;
        args.remove (0);
        const auto status = selfTest (args);
        std::_Exit (status);   // plugin libraries stay loaded; their teardown is not ours to run
    }

    if (args.size() < 2 || args.size() > 3)
        return fail (2, "usage: mdaw-plugin-scan <VST3|LV2|CLAP> <file-or-identifier> [<result-file>]\n"
                        "       mdaw-plugin-scan --self-test [<vst3 bundle> ...]");

    int status = 0;
    {
        // Plugins expect what a host has: the window-manager atoms (doc point 12), JUCE, and a
        // message thread, which this thread becomes. No audio device, no Tracktion engine.
        mdaw::defineWindowManagerAtoms();
        juce::initialiseJuce_GUI();
        juce::MessageManager::getInstance()->setCurrentThreadAsMessageThread();
        try
        {
            status = examine (args[0], args[1], args[2], resultFd);
        }
        catch (const std::exception& e)
        {
            status = fail (4, juce::String ("exception: ") + e.what());
        }
        catch (...)
        {
            status = fail (4, "unknown exception");
        }
    }
    // The result is written. Leave without running the plugins' static destructors or JUCE's
    // shutdown: a plugin that crashes there would turn a good result into a failure, and
    // nothing here needs tidying.
    std::fflush (stderr);
    std::_Exit (status);
}
} // namespace mdaw::bridge
