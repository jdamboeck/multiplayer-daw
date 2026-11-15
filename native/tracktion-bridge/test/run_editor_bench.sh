#!/bin/sh
# Builds and runs the editor benchmark (test/editor_bench.c) twice, from the repository root:
# first over a fresh settings folder (every installed plugin is examined: a cold scan), then
# again over the same folder (the scan uses what it learnt). Output goes to the terminal and to
# target/bench-scratch/bench-<timestamp>.txt.
#
# The windows open on a virtual X display (xvfb-run), never on the desktop. That display has no
# window manager, so the atoms JUCE only looks up (_NET_WM_WINDOW_TYPE, ...) do not exist there
# and X would end the program on the first window; they are made first with xprop, and the
# server is kept from resetting (which would drop them) with -noreset.
#
# Plugin folders: CLAP_PATH is searched IN ADDITION to ~/.clap, /usr/lib/clap and
# /usr/local/lib/clap (native/clap-format/clap_format.cpp, getDefaultLocationsToSearch), so the
# folder made here holds only test-gui.clap; the installed CLAP plugins are scanned where they
# are (copying them as well would list every one twice under one id). The bench itself copies
# bad-hang.clap into that folder for its scan part and removes it again. VST3 and LV2 are
# searched in JUCE's default folders (~/.vst3, /usr/lib/vst3, ~/.lv2, /usr/lib/lv2, ...).
set -eu

cd "$(dirname "$0")/../../.."
root=$PWD
build=target/native
scratch=$root/target/bench-scratch

# Plugins measured besides CLAP:org.mdaw.test-gui (FORMAT:ID@label; ids as mdaw_plugins_list
# reports them, which the bench prints).
plugins="
VST3:ABCDEF019182FAEB426C616D46743250@vst3_ft2
VST3:ABCDEF019182FAEB54737444564F5454@vst3_vitott
LV2:https://github.com/asb2m10/dexed@lv2_dexed
LV2:http://github.com/Chowdhury-DSP/ChowKick@lv2_chowkick
CLAP:org.chowdsp.ChowKick@clap_chowkick
"

cmake --build "$build" --target mdaw-editor-bench mdaw-plugin-scan mdaw-test-clap-gui mdaw-test-clap-hang

mkdir -p "$scratch"
rm -rf "$scratch/clap" "$scratch/settings-fresh"
mkdir -p "$scratch/clap"
cp "$build/tracktion-bridge/test-gui.clap" "$scratch/clap/"

atoms="WM_PROTOCOLS,WM_TAKE_FOCUS,WM_DELETE_WINDOW,WM_CHANGE_STATE,WM_STATE,_NET_WM_PING"
atoms="$atoms,_NET_WM_WINDOW_TYPE,_NET_WM_WINDOW_TYPE_NORMAL,_NET_WM_WINDOW_TYPE_COMBO"
atoms="$atoms,_NET_WM_STATE,_NET_WM_STATE_HIDDEN,_NET_WM_STATE_FULLSCREEN,_NET_WM_MOVERESIZE"
atoms="$atoms,_NET_FRAME_EXTENTS,_NET_WORKAREA,_MOTIF_WM_HINTS,_NET_WM_ALLOWED_ACTIONS"
atoms="$atoms,_NET_WM_ACTION_RESIZE,_NET_WM_ACTION_FULLSCREEN,_NET_WM_ACTION_MINIMIZE"
atoms="$atoms,_NET_WM_ACTION_CLOSE,_NET_WM_ACTION_MOVE"

out=$scratch/bench-$(date +%Y%m%d-%H%M%S).txt
export CLAP_PATH="$scratch/clap"
export MDAW_TRACE=1

run() {
    # $1: what this run is. Plugin chatter on stderr is left out of the file.
    echo "# ---- $1 ($(date '+%F %T'), $(git rev-parse --short HEAD))"
    # shellcheck disable=SC2086 # the plugin list is split on purpose
    timeout 900 xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" sh -c '
        xprop -root -f MDAW_BENCH_ATOMS 32a -set MDAW_BENCH_ATOMS "$0"
        exec "$@"' "$atoms" "$build/tracktion-bridge/mdaw-editor-bench" "$scratch/settings-fresh" $plugins \
        2> "$scratch/bench-stderr-$2.txt" \
        | grep -v '^# plugins: '
}

{
    run "cold: fresh settings folder" cold
    run "cached: same settings folder" cached
} | tee "$out"
echo "written to $out (stderr of the runs: $scratch/bench-stderr-*.txt)"
