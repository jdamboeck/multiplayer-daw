# The C ABI between the Rust application and Tracktion Engine (include/mdaw_engine.h).
#
# A static library that carries everything it needs: the bridge, and the JUCE and Tracktion
# module sources (they come in as interface sources of mdaw_engine_config). The engine
# libraries are linked PRIVATE, so a consumer gets the archive and the system libraries to link
# against, not a second copy of the module sources to compile. That is how a Rust binary will
# link it, and the C test below links it the same way.
add_library(mdaw_tracktion_bridge STATIC
    src/bridge.cpp
    src/engine_thread.cpp
    src/project.cpp)
target_include_directories(mdaw_tracktion_bridge PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(mdaw_tracktion_bridge PRIVATE mdaw_clap_format mdaw_engine_config)
set_target_properties(mdaw_tracktion_bridge PROPERTIES POSITION_INDEPENDENT_CODE ON)

# Exercises the bridge through the C header only, with no audio device.
add_executable(mdaw-bridge-test test/bridge_test.c)
set_target_properties(mdaw-bridge-test PROPERTIES C_STANDARD 11 C_STANDARD_REQUIRED ON)
target_link_libraries(mdaw-bridge-test PRIVATE mdaw_tracktion_bridge m)
