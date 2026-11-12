// mdaw-plugin-scan: examines one plugin file in a process of its own, so that a plugin that
// crashes or hangs while it is looked at takes only this process with it.
//
//   mdaw-plugin-scan <format> <file-or-identifier> [<result-file>]
//   mdaw-plugin-scan --self-test [<vst3 bundle> ...]
//
// <format> is "VST3", "LV2" or "CLAP"; <file-or-identifier> is one entry as that format's
// searchPathsForPlugins hands it out (a .vst3 bundle, a .clap file, an LV2 plugin URI).
//
// The result is an XML document:
//
//   <MDAW_PLUGIN_SCAN version="1" format="CLAP" file="...">
//     <FOUND id="org.example.synth"><PLUGIN .../></FOUND>     one per plugin in the file:
//     ...                                                      its own id (plugin_ids.h) and
//   </MDAW_PLUGIN_SCAN>                                        JUCE's description (createXml)
//
// Protocol. The result goes to <result-file> when one is given; that is how the bridge calls
// it. It is written to "<result-file>.part" and renamed when complete, so a file that exists is
// a whole result, and nothing a plugin prints can get into it (plugins do print while they are
// loaded). Without a result file it goes to stdout, for people; even then stdout carries only
// the result, because before anything is loaded the original stdout is set aside and file
// descriptor 1 is pointed at stderr. Exit status 0: examined, with zero or more plugins found.
// Non-zero: not examined, with the reason on stderr. A crash or hang is the parent's to see.
//
// --self-test checks the VST3 class-id byte-order conversion against the VST3 SDK's own
// layouts, and for each bundle given compares the ids from moduleinfo.json with those from the
// plugin's factory. Exit status 0 if everything agrees.
#pragma once

namespace mdaw::bridge
{
int pluginScanMain (int argc, char** argv);

/// The byte-order check of --self-test (vst3_id_check.cpp, which sees the VST3 SDK's
/// headers): prints what it compares, returns the number of failures.
int vst3IdByteOrderCheck();
}
