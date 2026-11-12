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
    void call (Step step);

    /// Runs the steps on the message thread one after the other, with `gapMs` of free message
    /// loop between them, and waits for the last. Calls from other threads wait until all steps
    /// are done. The first exception skips the remaining steps and is rethrown here.
    void call (std::vector<Step> steps, int gapMs);

    /// Runs the `tearDown` steps like call() (all of them, even after an exception), ends the
    /// dispatch loop, runs `afterLoop` on the thread outside any message callback, shuts JUCE
    /// down there and joins the thread.
    void stop (std::vector<Step> tearDown, int gapMs, Step afterLoop);

private:
    struct Job;
    std::shared_ptr<Job> post (std::vector<Step>, int gapMs, bool keepGoing);
    void startNext();
    void runStep();

    std::thread thread;
    Step afterLoop;

    // Message thread only
    std::deque<std::shared_ptr<Job>> waiting;
    std::shared_ptr<Job> current;
};
} // namespace mdaw::bridge
