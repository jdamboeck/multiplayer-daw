# Native side of mdaw: Tracktion Engine (pinned submodule, with its pinned JUCE) and what is
# built on it. See docs/tracktion-integration.md for dependencies and how to build.
cmake_minimum_required(VERSION 3.22)
project(mdaw_native VERSION 0.1.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE RelWithDebInfo)
endif()

# RelWithDebInfo with -g1 instead of -g: function names and line tables, so a crash in the
# plugin host still gives a backtrace with file:line, but no local variables or types. Full
# debug info made every Rust binary that links the bridge about 800 MB (some 240 MB of it from
# here) and target/debug grow to 60 GB. Set before JUCE and Tracktion are added, so they get it
# too; a normal variable, so it also wins over the -g already in an existing cache. That also
# means that while the option is off, CMAKE_C/CXX_FLAGS_RELWITHDEBINFO given with -D or set in
# the cache are ignored, though ccmake and CMakeCache.txt still show them: set
# MDAW_NATIVE_FULL_DEBUG=ON to build with your own flags.
option(MDAW_NATIVE_FULL_DEBUG "Build the native code with full debug info (-g)" OFF)
if(NOT MDAW_NATIVE_FULL_DEBUG AND NOT MSVC)
    set(CMAKE_C_FLAGS_RELWITHDEBINFO "-O2 -g1 -DNDEBUG")
    set(CMAKE_CXX_FLAGS_RELWITHDEBINFO "-O2 -g1 -DNDEBUG")
endif()

set(MDAW_TE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/third_party/tracktion_engine")
if(NOT EXISTS "${MDAW_TE_DIR}/modules/juce/CMakeLists.txt")
    message(FATAL_ERROR
        "Tracktion Engine and JUCE are missing. Fetch the pinned revisions with:\n"
        "  git -c url.https://github.com/.insteadOf=git@github.com: submodule update --init --recursive --depth 1")
endif()

add_subdirectory("${MDAW_TE_DIR}/modules/juce" juce EXCLUDE_FROM_ALL)
add_subdirectory("${MDAW_TE_DIR}/modules" tracktion EXCLUDE_FROM_ALL)

# What every target that compiles the JUCE and Tracktion modules is built with. One list, so
# that the spike and the bridge cannot drift apart.
add_library(mdaw_engine_config INTERFACE)
target_compile_definitions(mdaw_engine_config INTERFACE
    JUCE_PLUGINHOST_VST3=1
    JUCE_PLUGINHOST_LV2=1
    JUCE_PLUGINHOST_LADSPA=1
    JUCE_ALSA=1
    JUCE_JACK=1
    JUCE_USE_CURL=0
    JUCE_WEB_BROWSER=0
    JUCE_MODAL_LOOPS_PERMITTED=1
    JUCE_STRICT_REFCOUNTEDPOINTER=1
    JUCE_DISPLAY_SPLASH_SCREEN=0
    JUCE_REPORT_APP_USAGE=0
    TRACKTION_ENABLE_TIMESTRETCH_SOUNDTOUCH=1)
target_link_libraries(mdaw_engine_config INTERFACE
    tracktion::tracktion_core
    tracktion::tracktion_engine
    tracktion::tracktion_graph
    juce::juce_audio_devices
    juce::juce_audio_processors
    juce::juce_audio_utils
    juce::juce_recommended_config_flags)
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    target_link_libraries(mdaw_engine_config INTERFACE atomic)
endif()
if(CMAKE_CXX_COMPILER_ID MATCHES "GNU")
    target_compile_options(mdaw_engine_config INTERFACE -Wno-unknown-pragmas)
    # Tracktion v3.2.0 bundles nanorange, which has a typo in a template body that is never
    # instantiated. GCC 15 and later diagnose such bodies as errors; this turns that back into
    # the old behaviour without touching the pinned sources. Older GCC (13 and 14, as on Ubuntu)
    # does not know the flag, but GCC accepts any unknown -Wno- option silently, so the check
    # below passes and the flag is passed there too; it is only mentioned (as a warning, never an
    # error) when some other diagnostic is printed for the same file. We rely on that.
    include(CheckCXXCompilerFlag)
    check_cxx_compiler_flag(-Wno-template-body MDAW_HAS_WNO_TEMPLATE_BODY)
    if(MDAW_HAS_WNO_TEMPLATE_BODY)
        target_compile_options(mdaw_engine_config INTERFACE -Wno-template-body)
    endif()
endif()

add_subdirectory(clap-format)
add_subdirectory(tracktion-bridge)

option(MDAW_BUILD_SPIKE "Build the native Tracktion spike application" ON)
if(MDAW_BUILD_SPIKE)
    add_subdirectory(spike)
endif()
