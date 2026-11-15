#pragma once

namespace mdaw
{
    /// Call once before JUCE is initialised. JUCE looks up a number of window-manager atoms
    /// "only if they exist" and then uses some of them unchecked; on an X server where no
    /// program has defined them yet (XWayland under a minimal compositor such as niri, right
    /// after login) opening a window ends the process with BadAtom. This defines them. Does
    /// nothing where there is no X display.
    void defineWindowManagerAtoms();
}
