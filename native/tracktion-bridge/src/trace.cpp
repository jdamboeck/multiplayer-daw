#include "trace.h"

#include <cstdio>
#include <cstdlib>

namespace mdaw::bridge
{
bool traceOn()
{
    static const bool on = std::getenv ("MDAW_TRACE") != nullptr;
    return on;
}

void trace (const juce::String& text)
{
    if (! traceOn())
        return;
    const auto day = juce::Time::currentTimeMillis() % 86400000;
    // One fprintf per line, so lines from several threads do not mix.
    std::fprintf (stderr, "mdaw-trace %02d:%02d:%02d.%03d engine %s\n",
                  (int) (day / 3600000), (int) (day / 60000 % 60), (int) (day / 1000 % 60), (int) (day % 1000),
                  text.toRawUTF8());
}

double nowMs()
{
    return juce::Time::getMillisecondCounterHiRes();
}

juce::String ms (double milliseconds)
{
    return juce::String (milliseconds, 1);
}
} // namespace mdaw::bridge
