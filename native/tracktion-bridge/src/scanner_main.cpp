// The mdaw-plugin-scan executable. Everything is in the bridge library (scanner.cpp), which
// already carries JUCE and Tracktion compiled once; see scanner.h for the protocol.
#include "scanner.h"

int main (int argc, char** argv)
{
    return mdaw::bridge::pluginScanMain (argc, argv);
}
