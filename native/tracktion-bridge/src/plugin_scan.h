// The parent side of the plugin scan with a scanner (mdaw-plugin-scan, scanner.h): running one
// child per plugin file with a time limit, and remembering what became of each file.
#pragma once

#include <juce_audio_processors/juce_audio_processors.h>

#include <atomic>
#include <map>
#include <utility>
#include <vector>

namespace mdaw::bridge
{
/// How a child process ended.
struct ChildOutcome
{
    bool started = false;    // false: `error` says why
    bool exited = false;     // returned or called exit: `code`
    bool signalled = false;  // killed by a signal it did not catch (a crash): `signal`
    bool timedOut = false;   // still running at the deadline: killed
    bool cancelled = false;  // the cancel flag was set while it ran: killed
    int code = 0;
    int signal = 0;
    juce::String error;
};

/// Runs `exe` with `args`, stdin from /dev/null, stdout and stderr into `log`, and waits for it
/// for at most `timeoutMs`. Blocks the calling thread with plain waits, never a message loop
/// (doc point 14). A child still running at the deadline is killed with SIGKILL together with
/// everything in its process group (it is started as a group of its own), and always reaped:
/// no zombie and no stray grandchild is left behind. Inherits no file descriptor but 0, 1, 2.
/// `cancel`, if given, is looked at while waiting (every few milliseconds): once it is true the
/// child is killed and reaped the same way, and the outcome says `cancelled`.
ChildOutcome runChild (const juce::File& exe, const juce::StringArray& args, const juce::File& log, int timeoutMs,
                       const std::atomic<bool>* cancel = nullptr);

/// What the scanner's limit is: MDAW_SCAN_TIMEOUT_MS if set to a positive number (tests),
/// otherwise 30 s.
int scanTimeoutMs();

/// When a plugin file last changed: its modification time, or for a bundle (a folder) the
/// newest one of everything in it, so that a replaced library inside a VST3 bundle counts.
juce::int64 stampOf (const juce::File&);

/// What became of every file the scanner examined: kept in the settings folder, so that a
/// file that crashed or hung the scanner is not examined again until it changes.
class ScanRecords
{
public:
    explicit ScanRecords (juce::File storeFile);

    struct Entry
    {
        juce::int64 stamp = 0;   // stampOf the file when it was examined
        bool ok = false;
        juce::String reason;     // why not ok
    };

    const Entry* find (const juce::String& format, const juce::String& file) const;
    void put (const juce::String& format, const juce::String& file, Entry);
    /// Forgets the files of `format` that are not in `present`.
    void keepOnly (const juce::String& format, const juce::StringArray& present);
    /// Forgets every file of `format`.
    void forget (const juce::String& format);

    using Key = std::pair<juce::String, juce::String>;   // format, file
    const std::map<Key, Entry>& all() const { return entries; }

    void save();

private:
    juce::File file;
    std::map<Key, Entry> entries;
    bool dirty = false;
};

/// What the scanner made of one candidate file. Plain data, made on the thread that ran the
/// scanner; the message thread merges it into the known-plugin list, the ids and the records.
struct Examined
{
    juce::String startError;     // not empty: the scanner could not be started
    bool cancelled = false;      // killed because the scan was cancelled: nothing to merge
    ScanRecords::Entry entry;    // what the records get for the file
    /// What was found: JUCE's description and the plugin's own id, only those whose format and
    /// file are the ones asked for and whose id could be read.
    std::vector<std::pair<juce::PluginDescription, juce::String>> found;
    double childMs = 0;          // how long the scanner ran
};

/// Runs the scanner on one candidate (format, file) with `result` and `log` as its files, and
/// reads what it wrote. Touches nothing but those files: safe on any thread. `stamp` goes into
/// the entry.
Examined examineInChild (const juce::File& scanner, const juce::String& format, const juce::String& file,
                         juce::int64 stamp, const juce::File& result, const juce::File& log, int timeoutMs,
                         const std::atomic<bool>* cancel);
} // namespace mdaw::bridge
