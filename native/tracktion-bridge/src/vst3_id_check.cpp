// The byte-order check of `mdaw-plugin-scan --self-test`. Compiled with the VST3 SDK bundled
// with JUCE on its include path (CMakeLists.txt), so the Windows (COM) layout of a TUID comes
// from the SDK's own code, VST3::UID (uid.h), which builds it with the same byte order as
// INLINE_UID under COM_COMPATIBLE and does so on any platform. Runs on Linux.
#include <public.sdk/source/vst/utility/uid.h>

#include "plugin_ids.h"
#include "scanner.h"

#include <cstdio>
#include <string>

namespace mdaw::bridge
{
int vst3IdByteOrderCheck()
{
    int failures = 0;
    auto check = [&] (bool ok, const char* what, const juce::String& got)
    {
        std::printf ("%s %s: %s\n", ok ? "ok  " : "FAIL", what, got.toRawUTF8());
        failures += ok ? 0 : 1;
    };

    // l1..l4 with every byte distinct, so that any misplaced byte shows.
    const std::string canonical = "0123456789ABCDEFFEDCBA9876543210";
    const VST3::UID com (0x01234567u, 0x89ABCDEFu, 0xFEDCBA98u, 0x76543210u, true);
    const VST3::UID plain (0x01234567u, 0x89ABCDEFu, 0xFEDCBA98u, 0x76543210u, false);
    const auto* comBytes = reinterpret_cast<const std::uint8_t*> (com.data());
    const auto* plainBytes = reinterpret_cast<const std::uint8_t*> (plain.data());

    check (juce::String::toHexString (comBytes, 16, 0).toUpperCase() != juce::String (canonical),
           "the SDK's COM layout differs from the canonical order", juce::String::toHexString (comBytes, 16, 0).toUpperCase());
    check (canonicalVst3Id (comBytes, true) == juce::String (canonical), "COM layout to canonical", canonicalVst3Id (comBytes, true));
    check (canonicalVst3Id (plainBytes, false) == juce::String (canonical), "plain layout to canonical", canonicalVst3Id (plainBytes, false));
    // The SDK's own text of the COM layout (UID::toString with comFormat, as moduleinfo.json
    // is written on Windows) is the same canonical text.
    check (com.toString (true) == canonical, "SDK prints the COM layout canonically", com.toString (true));
    check (plain.toString (false) == canonical, "SDK prints the plain layout canonically", plain.toString (false));
    // And reading the canonical text back as the SDK does on Windows gives the COM bytes.
    const auto parsed = VST3::UID::fromString (canonical, true);
    check (parsed && std::memcmp (parsed->data(), com.data(), 16) == 0, "SDK parses canonical text into the COM layout",
           parsed ? canonicalVst3Id (reinterpret_cast<const std::uint8_t*> (parsed->data()), true) : juce::String ("(none)"));

    // JUCE's uniqueId: getHashForRange over the FUID's four longs.
    std::uint32_t hash = 0;
    for (auto l : { 0x01234567u, 0x89ABCDEFu, 0xFEDCBA98u, 0x76543210u })
        hash = hash * 31 + l;
    check (juceUniqueIdOfVst3Id (canonical) == (int) hash, "uniqueId of the canonical id", juce::String (juceUniqueIdOfVst3Id (canonical)));
    return failures;
}
} // namespace mdaw::bridge
