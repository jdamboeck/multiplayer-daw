// The engine's message thread: one thread that is JUCE's message thread for the engine's whole
// life. Tracktion does its housekeeping (graph rebuilds, device changes, plugin loading, timers)
// on that thread, so every call from the application is carried out there.
//
// Nothing here runs a dispatch loop from inside a message callback. On Linux, JUCE's run loop
// keeps the callbacks it is dispatching in a member vector that a nested dispatch clears and
// refills, so a nested loop corrupts the outer one (seen: the outer loop then sleeps through
// the quit message and never returns). Work that needs the loop to run in the middle (edit
// teardown) is given as steps with a gap between them instead.
#pragma once

#include <deque>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

namespace mdaw::bridge
{
class EngineThread
{
public:
    using Step = std::function<void()>;

    EngineThread() = default;
    ~EngineThread();

    /// Starts the thread, initialises JUCE on it, runs `setUp` there and then the dispatch loop.
    /// Returns once `setUp` has run; rethrows what it threw (the thread has then ended).
    void start (Step setUp);

    /// Runs `step` on the message thread and waits for it. Rethrows what it threw.
    ///
    /// Every call is timed: how long it waited for the message thread (from being posted to its
    /// first step starting there) and how long its steps ran there. With tracing on (trace.h) a
    /// call with a `label` is always traced, any other one when either time reaches 50 ms (as
    /// "call"). Either time reaching 100 ms is written to the engine's log, tracing or not.
    /// `label` is a string literal: it is kept until the call is over.
    void call (Step step, const char* label = nullptr);

    /// Runs the steps on the message thread one after the other, with `gapMs` of free message
    /// loop between them, and waits for the last. Calls from other threads wait until all steps
    /// are done. The first exception skips the remaining steps and is rethrown here. Timed as
    /// the call above; the run time is that of the steps, without the gaps.
    void call (std::vector<Step> steps, int gapMs, const char* label = nullptr);

    /// Runs `step` on the message thread as soon as it is free, without waiting for it. It runs
    /// between the steps of a call (steps, gapMs) if one is in progress, and after whatever was
    /// posted before it. What it throws is dropped. False if the message thread does not run.
    bool post (Step step);

    /// Runs the `tearDown` steps like call() (all of them, even after an exception), ends the
    /// dispatch loop, runs `afterLoop` on the thread outside any message callback, shuts JUCE
    /// down there and joins the thread.
    void stop (std::vector<Step> tearDown, int gapMs, Step afterLoop);

private:
    struct Job;
    std::shared_ptr<Job> post (std::vector<Step>, int gapMs, bool keepGoing, const char* label);
    void startNext();
    void runStep();

    std::thread thread;
    Step afterLoop;

    // Message thread only
    std::deque<std::shared_ptr<Job>> waiting;
    std::shared_ptr<Job> current;
};
} // namespace mdaw::bridge
