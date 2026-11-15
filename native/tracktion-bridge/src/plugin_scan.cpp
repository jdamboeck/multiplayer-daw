#include "plugin_scan.h"
#include "trace.h"

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#if JUCE_LINUX || JUCE_MAC || JUCE_BSD
 #include <cerrno>
 #include <fcntl.h>
 #include <signal.h>
 #include <spawn.h>
 #include <sys/wait.h>
 #include <unistd.h>
extern char** environ;
#endif

namespace mdaw::bridge
{
#if JUCE_LINUX || JUCE_MAC || JUCE_BSD
ChildOutcome runChild (const juce::File& exe, const juce::StringArray& args, const juce::File& log, int timeoutMs,
                       const std::atomic<bool>* cancel)
{
    ChildOutcome out;
    std::vector<std::string> strings { exe.getFullPathName().toStdString() };
    for (auto& a : args)
        strings.push_back (a.toStdString());
    std::vector<char*> argv;
    for (auto& s : strings)
        argv.push_back (s.data());
    argv.push_back (nullptr);
    const auto logPath = log.getFullPathName().toStdString();

    posix_spawn_file_actions_t actions;
    posix_spawn_file_actions_init (&actions);
    posix_spawn_file_actions_addopen (&actions, 0, "/dev/null", O_RDONLY, 0);
    posix_spawn_file_actions_addopen (&actions, 1, logPath.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    posix_spawn_file_actions_adddup2 (&actions, 1, 2);
   #if defined (__GLIBC__) && (__GLIBC__ > 2 || (__GLIBC__ == 2 && __GLIBC_MINOR__ >= 34))
    // Not the engine's audio device, X connection or anything else it has open.
    posix_spawn_file_actions_addclosefrom_np (&actions, 3);
   #endif
    posix_spawnattr_t attributes;
    posix_spawnattr_init (&attributes);
    sigset_t none, all;
    sigemptyset (&none);
    sigfillset (&all);
    posix_spawnattr_setsigmask (&attributes, &none);
    posix_spawnattr_setsigdefault (&attributes, &all);
    posix_spawnattr_setpgroup (&attributes, 0);   // a process group of its own, to kill as a whole
    posix_spawnattr_setflags (&attributes, POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);

    pid_t pid = 0;
    const int error = posix_spawn (&pid, argv[0], &actions, &attributes, argv.data(), environ);
    posix_spawn_file_actions_destroy (&actions);
    posix_spawnattr_destroy (&attributes);
    if (error != 0)
    {
        out.error = std::strerror (error);
        return out;
    }
    out.started = true;

    // Waits until the child has ended, without reaping it yet (WNOWAIT): while it is not
    // reaped its process id, and so its group id, cannot be given to anyone else, so killing
    // the group afterwards cannot hit an unrelated process.
    const auto deadline = juce::Time::getMillisecondCounterHiRes() + timeoutMs;
    bool statusLost = false;
    for (;;)
    {
        siginfo_t info {};
        if (waitid (P_PID, (id_t) pid, &info, WEXITED | WNOHANG | WNOWAIT) == 0)
        {
            if (info.si_pid == pid)
                break;
        }
        else if (errno != EINTR)
        {
            // ECHILD: the application ignores SIGCHLD, so the system reaps the child and its
            // status is lost. Then all there is to watch is whether it still exists.
            statusLost = true;
            if (::kill (pid, 0) != 0)
                break;
        }
        if (cancel != nullptr && cancel->load())
        {
            out.cancelled = true;
            break;
        }
        if (juce::Time::getMillisecondCounterHiRes() >= deadline)
        {
            out.timedOut = true;
            break;
        }
        std::this_thread::sleep_for (std::chrono::milliseconds (5));
    }

    // The child (if it hung or the scan was cancelled) and anything it started.
    if (! statusLost || out.timedOut || out.cancelled)
        ::kill (-pid, SIGKILL);
    int status = 0;
    pid_t reaped = 0;
    do
        reaped = statusLost ? -1 : waitpid (pid, &status, 0);
    while (reaped < 0 && errno == EINTR && ! statusLost);

    if (out.timedOut || out.cancelled || reaped != pid)
    {
        if (! out.timedOut && ! out.cancelled)
            out.error = "its exit status is not available (the application ignores SIGCHLD)";
        return out;
    }
    if (WIFEXITED (status))
    {
        out.exited = true;
        out.code = WEXITSTATUS (status);
    }
    else if (WIFSIGNALED (status))
    {
        out.signalled = true;
        out.signal = WTERMSIG (status);
    }
    return out;
}
#else
// Windows: juce::ChildProcess. Compiled for, NOT tested (no such machine here). It kills only
// the child itself, not processes the child started. Waits in short slices, so that the cancel
// flag is seen.
ChildOutcome runChild (const juce::File& exe, const juce::StringArray& args, const juce::File& log, int timeoutMs,
                       const std::atomic<bool>* cancel)
{
    juce::ignoreUnused (log);
    ChildOutcome out;
    juce::StringArray command (exe.getFullPathName());
    command.addArray (args);
    juce::ChildProcess child;
    if (! child.start (command, 0))
    {
        out.error = "could not start " + exe.getFullPathName();
        return out;
    }
    out.started = true;
    const auto deadline = juce::Time::getMillisecondCounterHiRes() + timeoutMs;
    while (! child.waitForProcessToFinish (20))
    {
        if (cancel != nullptr && cancel->load())
            out.cancelled = true;
        else if (juce::Time::getMillisecondCounterHiRes() >= deadline)
            out.timedOut = true;
        else
            continue;
        child.kill();
        return out;
    }
    const auto code = child.getExitCode();
    // NTSTATUS codes (0xC0000005 access violation, ...) are what a crash leaves.
    if (code >= 0xC0000000u)
    {
        out.signalled = true;
        out.signal = (int) code;
    }
    else
    {
        out.exited = true;
        out.code = (int) code;
    }
    return out;
}
#endif

namespace
{
/// The last line the scanner wrote to stderr (it says why it gave up), shortened.
juce::String lastLineOf (const juce::File& log)
{
    auto lines = juce::StringArray::fromLines (log.loadFileAsString());
    lines.removeEmptyStrings();
    auto last = lines.isEmpty() ? juce::String ("(nothing)") : lines[lines.size() - 1].trim();
    return last.length() > 300 ? last.substring (0, 300) + "..." : last;
}

juce::String signalName (int signal)
{
   #if JUCE_LINUX || JUCE_MAC || JUCE_BSD
    if (const char* name = strsignal (signal))
        return juce::String (signal) + ", " + name;
   #endif
    return juce::String (signal);
}
} // namespace

Examined examineInChild (const juce::File& scanner, const juce::String& format, const juce::String& file,
                         juce::int64 stamp, const juce::File& result, const juce::File& log, int timeoutMs,
                         const std::atomic<bool>* cancel)
{
    Examined out;
    result.deleteFile();
    const auto started = nowMs();
    const auto outcome = runChild (scanner, { format, file, result.getFullPathName() }, log, timeoutMs, cancel);
    out.childMs = nowMs() - started;
    if (! outcome.started)
    {
        out.startError = outcome.error;
        return out;
    }
    if (outcome.cancelled)
    {
        out.cancelled = true;
        return out;
    }

    auto& entry = out.entry;
    entry = { stamp, false, {} };
    if (outcome.timedOut)
        entry.reason = "hung: no result within " + juce::String (timeoutMs) + " ms, so the scanner was stopped";
    else if (outcome.signalled)
        entry.reason = "crashed while being examined (signal " + signalName (outcome.signal) + ")";
    else if (outcome.exited && outcome.code != 0)
        entry.reason = "the scanner could not examine it (exit status " + juce::String (outcome.code) + "): " + lastLineOf (log);
    else if (! outcome.exited && ! result.existsAsFile())
        entry.reason = "the scanner ended without a result; " + outcome.error;
    else if (auto xml = juce::XmlDocument::parse (result); xml == nullptr || ! xml->hasTagName ("MDAW_PLUGIN_SCAN"))
        entry.reason = "the scanner gave no readable result";
    else
    {
        // Only plugins whose own id could be read are of use: they are named by it.
        for (auto* found : xml->getChildWithTagNameIterator ("FOUND"))
        {
            juce::PluginDescription d;
            const auto id = found->getStringAttribute ("id");
            if (auto* p = found->getChildByName ("PLUGIN"); p != nullptr && d.loadFromXml (*p) && id.isNotEmpty()
                                                            && d.pluginFormatName == format && d.fileOrIdentifier == file)
            {
                out.found.emplace_back (d, id);
                entry.ok = true;
            }
        }
        if (! entry.ok)
            entry.reason = "no plugin found in it (not a plugin, or it failed to load)";
    }
    return out;
}

int scanTimeoutMs()
{
    const auto text = juce::SystemStats::getEnvironmentVariable ("MDAW_SCAN_TIMEOUT_MS", {});
    const auto value = text.getIntValue();
    return text.containsOnly ("0123456789") && value > 0 ? value : 30000;
}

juce::int64 stampOf (const juce::File& f)
{
    auto newest = f.getLastModificationTime().toMilliseconds();
    if (f.isDirectory())
        for (const auto& entry : juce::RangedDirectoryIterator (f, true, "*", juce::File::findFilesAndDirectories))
            newest = std::max (newest, entry.getModificationTime().toMilliseconds());
    return newest;
}

//==============================================================================
ScanRecords::ScanRecords (juce::File storeFile) : file (std::move (storeFile))
{
    if (auto xml = juce::XmlDocument::parse (file))
        for (auto* e : xml->getChildWithTagNameIterator ("FILE"))
        {
            Entry entry;
            entry.stamp = e->getStringAttribute ("stamp").getHexValue64();
            entry.ok = e->getBoolAttribute ("ok");
            entry.reason = e->getStringAttribute ("reason");
            const auto format = e->getStringAttribute ("format"), path = e->getStringAttribute ("file");
            if (format.isNotEmpty() && path.isNotEmpty())
                entries[{ format, path }] = entry;
        }
}

const ScanRecords::Entry* ScanRecords::find (const juce::String& format, const juce::String& path) const
{
    auto found = entries.find ({ format, path });
    return found != entries.end() ? &found->second : nullptr;
}

void ScanRecords::put (const juce::String& format, const juce::String& path, Entry entry)
{
    entries[{ format, path }] = std::move (entry);
    dirty = true;
}

void ScanRecords::keepOnly (const juce::String& format, const juce::StringArray& present)
{
    for (auto it = entries.begin(); it != entries.end();)
    {
        if (it->first.first == format && ! present.contains (it->first.second))
        {
            it = entries.erase (it);
            dirty = true;
        }
        else
        {
            ++it;
        }
    }
}

void ScanRecords::forget (const juce::String& format)
{
    keepOnly (format, {});
}

void ScanRecords::save()
{
    if (! std::exchange (dirty, false))
        return;
    juce::XmlElement root ("MDAW_SCAN_RECORDS");
    for (auto& [key, entry] : entries)
    {
        auto* e = root.createNewChildElement ("FILE");
        e->setAttribute ("format", key.first);
        e->setAttribute ("file", key.second);
        e->setAttribute ("stamp", juce::String::toHexString (entry.stamp));
        e->setAttribute ("ok", entry.ok);
        if (entry.reason.isNotEmpty())
            e->setAttribute ("reason", entry.reason);
    }
    root.writeTo (file);
}
} // namespace mdaw::bridge
