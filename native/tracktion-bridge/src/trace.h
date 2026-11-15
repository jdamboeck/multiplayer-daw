// Timings of what the engine does, for finding where time goes. Off unless the environment
// variable MDAW_TRACE is set; then each line goes to stderr in the format of the Rust client's
// trace (crates/mdaw-client/src/trace.rs):
//
//   mdaw-trace HH:MM:SS.mmm engine <text>
//
// The time of day comes from the wall clock (UTC, to line up with the client's lines);
// durations are measured with the monotonic clock, nowMs(). Never called on the audio thread.
#pragma once

#include <juce_core/juce_core.h>

namespace mdaw::bridge
{
/// Whether MDAW_TRACE is set; read once.
bool traceOn();

/// Writes one line if tracing is on.
void trace (const juce::String& text);

/// Milliseconds of a monotonic clock, with fractions.
double nowMs();

/// A duration for a trace or log line: "12.3".
juce::String ms (double milliseconds);
} // namespace mdaw::bridge
