#include "plugin_scan.h"

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
ChildOutcome runChild (const juce::File& exe, const juce::StringArray& args, const juce::File& log, int timeoutMs)
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
        if (juce::Time::getMillisecondCounterHiRes() >= deadline)
        {
            out.timedOut = true;
            break;
        }
        std::this_thread::sleep_for (std::chrono::milliseconds (5));
    }

    // The child (if it hung) and anything it started.
    if (! statusLost || out.timedOut)
        ::kill (-pid, SIGKILL);
    int status = 0;
    pid_t reaped = 0;
    do
        reaped = statusLost ? -1 : waitpid (pid, &status, 0);
    while (reaped < 0 && errno == EINTR && ! statusLost);

    if (out.timedOut || reaped != pid)
    {
        if (! out.timedOut)
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
// the child itself, not processes the child started.
ChildOutcome runChild (const juce::File& exe, const juce::StringArray& args, const juce::File& log, int timeoutMs)
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
    if (! child.waitForProcessToFinish (timeoutMs))
    {
        out.timedOut = true;
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
