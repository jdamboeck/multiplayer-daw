#include "x11_atoms.h"

#if defined(__linux__)
#include <dlfcn.h>
#include <initializer_list>

void mdaw::defineWindowManagerAtoms()
{
    // libX11 by hand, as JUCE loads it: no build-time dependency on its headers.
    void* lib = dlopen ("libX11.so.6", RTLD_NOW | RTLD_LOCAL);
    if (lib == nullptr) return;
    auto open   = (void* (*) (const char*))             dlsym (lib, "XOpenDisplay");
    auto intern = (unsigned long (*) (void*, const char*, int)) dlsym (lib, "XInternAtom");
    auto close  = (int (*) (void*))                     dlsym (lib, "XCloseDisplay");
    if (open == nullptr || intern == nullptr || close == nullptr) return;
    void* display = open (nullptr);
    if (display == nullptr) return;
    // Every atom JUCE 8.0.6 asks for with "only if it exists". Atoms outlive the connection.
    for (const char* name : { "WM_PROTOCOLS", "WM_TAKE_FOCUS", "WM_DELETE_WINDOW", "_NET_WM_PING",
                              "WM_CHANGE_STATE", "WM_STATE", "_NET_WM_WINDOW_TYPE", "_NET_WM_STATE",
                              "_NET_WM_STATE_HIDDEN", "_NET_WM_STATE_FULLSCREEN", "_NET_WM_MOVERESIZE",
                              "_NET_FRAME_EXTENTS", "_NET_WORKAREA", "_MOTIF_WM_HINTS", "_WIN_HINTS",
                              "KWM_WIN_DECORATION", "_KDE_NET_WM_WINDOW_TYPE_OVERRIDE",
                              "_NET_WM_ALLOWED_ACTIONS", "_NET_WM_WINDOW_TYPE_NORMAL",
                              "_NET_WM_WINDOW_TYPE_COMBO" })
        intern (display, name, 0);
    close (display);
}
#else
void mdaw::defineWindowManagerAtoms() {}
#endif
