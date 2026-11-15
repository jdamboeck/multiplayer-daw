#!/bin/sh
# Runs a command on a virtual X display (xvfb-run), never on the desktop, with MDAW_TEST_GUI=1:
# by default the bridge's C test (test/bridge_test.c) with its plugin-window checks, from the
# repository root:
#
#   native/tracktion-bridge/test/run_gui_tests.sh                 the C test
#   native/tracktion-bridge/test/run_gui_tests.sh cargo test ...  anything else
#
# The virtual display has no window manager, so the atoms JUCE only looks up (_NET_WM_WINDOW_TYPE,
# ...) do not exist there and X would end the program on the first window; they are made first
# with xprop, and the server is kept from resetting (which would drop them) with -noreset. The same
# as test/run_editor_bench.sh.
#
# For the C test CLAP_PATH defaults to target/bridge-scratch/clap (where the test synth is put,
# see docs/tracktion-integration.md "The bridge").
set -eu

cd "$(dirname "$0")/../../.."
if [ $# -eq 0 ]; then
    : "${CLAP_PATH:=$PWD/target/bridge-scratch/clap}"
    export CLAP_PATH
    set -- target/native/tracktion-bridge/mdaw-bridge-test
fi

atoms="WM_PROTOCOLS,WM_TAKE_FOCUS,WM_DELETE_WINDOW,WM_CHANGE_STATE,WM_STATE,_NET_WM_PING"
atoms="$atoms,_NET_WM_WINDOW_TYPE,_NET_WM_WINDOW_TYPE_NORMAL,_NET_WM_WINDOW_TYPE_COMBO"
atoms="$atoms,_NET_WM_STATE,_NET_WM_STATE_HIDDEN,_NET_WM_STATE_FULLSCREEN,_NET_WM_MOVERESIZE"
atoms="$atoms,_NET_FRAME_EXTENTS,_NET_WORKAREA,_MOTIF_WM_HINTS,_NET_WM_ALLOWED_ACTIONS"
atoms="$atoms,_NET_WM_ACTION_RESIZE,_NET_WM_ACTION_FULLSCREEN,_NET_WM_ACTION_MINIMIZE"
atoms="$atoms,_NET_WM_ACTION_CLOSE,_NET_WM_ACTION_MOVE"

export MDAW_TEST_GUI=1
exec xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" sh -c '
    xprop -root -f MDAW_TEST_ATOMS 32a -set MDAW_TEST_ATOMS "$0"
    exec "$@"' "$atoms" "$@"
