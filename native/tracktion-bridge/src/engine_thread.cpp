#include "engine_thread.h"
#include "trace.h"
#include "x11_atoms.h"

#include <juce_gui_basics/juce_gui_basics.h>

#include <future>
#include <stdexcept>

namespace mdaw::bridge
{
struct EngineThread::Job
{
    std::vector<Step> steps;
    int gapMs = 0;
    bool keepGoing = false;   // run the remaining steps after one has thrown
    size_t next = 0;
    std::exception_ptr failure;
    std::promise<void> done;

    // Timings (nowMs): written on the message thread before `done` is set.
    const char* label = nullptr;
    double postedAt = 0, startedAt = 0, runMs = 0;
};

namespace
{
/// The call's timings: traced and logged as EngineThread::call says.
void report (const char* label, double postedAt, double startedAt, double runMs)
{
    if (startedAt <= 0)
        return;   // never ran
    const auto wait = startedAt - postedAt;
    const juce::String name = label != nullptr ? label : "call";
    const auto text = name + ": waited " + ms (wait) + " ms, ran " + ms (runMs) + " ms";
    if (traceOn() && (label != nullptr || wait >= 50 || runMs >= 50))
        trace (text);
    if (wait >= 100 || runMs >= 100)
        juce::Logger::writeToLog ("slow engine call: " + text);
}
} // namespace

EngineThread::~EngineThread()
{
    // stop() joins; a thread still running here means the owner never stopped it.
    if (thread.joinable())
        thread.detach();
}

void EngineThread::start (Step setUp)
{
    // Shared, so that the thread may still be inside set_value() when start() returns.
    auto ready = std::make_shared<std::promise<void>>();
    auto started = ready->get_future();

    thread = std::thread ([this, setUp = std::move (setUp), ready]
    {
        mdaw::defineWindowManagerAtoms();
        juce::initialiseJuce_GUI();
        juce::MessageManager::getInstance()->setCurrentThreadAsMessageThread();

        bool ok = false;
        try
        {
            setUp();
            ok = true;
            ready->set_value();
        }
        catch (...)
        {
            ready->set_exception (std::current_exception());
        }

        if (ok)
        {
            juce::MessageManager::getInstance()->runDispatchLoop();

            // Outside any message callback, so whatever runs a dispatch loop of its own here
            // (Tracktion's plugin manager does when it is deleted) cannot upset ours.
            if (afterLoop != nullptr)
            {
                try { afterLoop(); }
                catch (...) {}
            }
        }

        juce::shutdownJuce_GUI();
    });

    try
    {
        started.get();
    }
    catch (...)
    {
        thread.join();
        throw;
    }
}

std::shared_ptr<EngineThread::Job> EngineThread::post (std::vector<Step> steps, int gapMs, bool keepGoing, const char* label)
{
    auto job = std::make_shared<Job>();
    job->steps = std::move (steps);
    job->gapMs = gapMs;
    job->keepGoing = keepGoing;
    job->label = label;
    job->postedAt = nowMs();

    if (! juce::MessageManager::callAsync ([this, job]
                                           {
                                               waiting.push_back (job);
                                               startNext();
                                           }))
        job->done.set_exception (std::make_exception_ptr (std::runtime_error ("the engine's message thread is not running")));
    return job;
}

bool EngineThread::post (Step step)
{
    return juce::MessageManager::callAsync ([step = std::move (step)]
                                            {
                                                try { step(); }
                                                catch (...) {}
                                            });
}

void EngineThread::startNext()
{
    if (current != nullptr || waiting.empty())
        return;
    current = waiting.front();
    waiting.pop_front();
    runStep();
}

void EngineThread::runStep()
{
    auto job = current;
    while (job->next < job->steps.size())
    {
        const auto stepStart = nowMs();
        if (job->startedAt <= 0)
            job->startedAt = stepStart;
        try
        {
            job->steps[job->next++]();
        }
        catch (...)
        {
            if (job->failure == nullptr)
                job->failure = std::current_exception();
            if (! job->keepGoing)
                job->next = job->steps.size();
        }
        job->runMs += nowMs() - stepStart;

        if (job->next < job->steps.size() && job->gapMs > 0)
        {
            juce::Timer::callAfterDelay (job->gapMs, [this] { runStep(); });
            return;
        }
    }

    current = nullptr;
    if (job->failure != nullptr)
        job->done.set_exception (job->failure);
    else
        job->done.set_value();

    // The next job gets a message of its own, after whatever this one left posted.
    if (! waiting.empty())
        juce::MessageManager::callAsync ([this] { startNext(); });
}

void EngineThread::call (Step step, const char* label)
{
    std::vector<Step> steps;
    steps.push_back (std::move (step));
    call (std::move (steps), 0, label);
}

void EngineThread::call (std::vector<Step> steps, int gapMs, const char* label)
{
    auto job = post (std::move (steps), gapMs, false, label);
    std::exception_ptr failure;
    try
    {
        job->done.get_future().get();
    }
    catch (...)
    {
        failure = std::current_exception();
    }
    report (job->label, job->postedAt, job->startedAt, job->runMs);
    if (failure != nullptr)
        std::rethrow_exception (failure);
}

void EngineThread::stop (std::vector<Step> tearDown, int gapMs, Step after)
{
    if (! thread.joinable())
        return;

    afterLoop = std::move (after);
    tearDown.push_back ([] { juce::MessageManager::getInstance()->stopDispatchLoop(); });

    std::exception_ptr failure;
    try
    {
        post (std::move (tearDown), gapMs, true, nullptr)->done.get_future().get();
    }
    catch (...)
    {
        failure = std::current_exception();
    }

    thread.join();

    if (failure != nullptr)
        std::rethrow_exception (failure);
}
} // namespace mdaw::bridge
