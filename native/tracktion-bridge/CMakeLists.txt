# The C ABI between the Rust application and Tracktion Engine (include/mdaw_engine.h).
#
# A static library holding the bridge and the JUCE and Tracktion module sources (they come in
# as interface sources of mdaw_engine_config). The engine libraries are linked PRIVATE, so a
# consumer links two archives — this one and mdaw_clap_format — plus system libraries, and
# compiles no module source again. The Rust build script and the C test below link it so.
add_library(mdaw_tracktion_bridge STATIC
    src/bridge.cpp
    src/engine_thread.cpp
    src/plugin_ids.cpp
    src/plugin_scan.cpp
    src/project.cpp
    src/scanner.cpp
    src/trace.cpp
    src/vst3_id_check.cpp)
target_include_directories(mdaw_tracktion_bridge PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(mdaw_tracktion_bridge PRIVATE mdaw_clap_format mdaw_engine_config)
set_target_properties(mdaw_tracktion_bridge PROPERTIES POSITION_INDEPENDENT_CODE ON)
# Only the VST3 byte-order check sees the VST3 SDK that comes with JUCE.
set_source_files_properties(src/vst3_id_check.cpp PROPERTIES INCLUDE_DIRECTORIES
    "${MDAW_TE_DIR}/modules/juce/modules/juce_audio_processors/format_types/VST3_SDK")

# Examines one plugin file in a process of its own (src/scanner.h). Its code is in the library
# above, so it is linked like the C test and compiles no JUCE module again.
add_executable(mdaw-plugin-scan src/scanner_main.cpp)
target_link_libraries(mdaw-plugin-scan PRIVATE mdaw_tracktion_bridge)

# CLAP plugins the C test needs (test/bad_clap.c): one that crashes and one that hangs while
# being examined, and one that saves no state; and one with a window and a state whose speed
# the environment sets, for the editor benchmarks. Named <name>.clap in this build folder.
foreach(variant crash hang stateless gui)
    string(TOUPPER ${variant} upper)
    set(define MDAW_BAD_${upper})
    if(variant STREQUAL "stateless")
        set(name test-stateless)
    elseif(variant STREQUAL "gui")
        set(name test-gui)
        set(define MDAW_TEST_GUI)
    else()
        set(name bad-${variant})
    endif()
    add_library(mdaw-test-clap-${variant} MODULE test/bad_clap.c)
    target_include_directories(mdaw-test-clap-${variant} PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/clap/include)
    target_compile_definitions(mdaw-test-clap-${variant} PRIVATE ${define}=1)
    set_target_properties(mdaw-test-clap-${variant} PROPERTIES
        C_STANDARD 11 C_STANDARD_REQUIRED ON PREFIX "" OUTPUT_NAME ${name} SUFFIX ".clap"
        C_VISIBILITY_PRESET hidden)
endforeach()

# Exercises the bridge through the C header only, with no audio device. It finds the scanner
# and the test plugins at the paths compiled in here (overridable, see the test).
add_executable(mdaw-bridge-test test/bridge_test.c)
set_target_properties(mdaw-bridge-test PROPERTIES C_STANDARD 11 C_STANDARD_REQUIRED ON)
target_link_libraries(mdaw-bridge-test PRIVATE mdaw_tracktion_bridge m pthread)
target_compile_definitions(mdaw-bridge-test PRIVATE
    MDAW_TEST_SCANNER="$<TARGET_FILE:mdaw-plugin-scan>"
    MDAW_TEST_CLAP_DIR="$<TARGET_FILE_DIR:mdaw-test-clap-crash>")
add_dependencies(mdaw-bridge-test mdaw-plugin-scan mdaw-test-clap-crash mdaw-test-clap-hang mdaw-test-clap-stateless
                 mdaw-test-clap-gui)

# Times the calls behind a plugin's window (adding a device, opening and closing its editor,
# reading its state) and how the engine's thread answers while a scan runs, through the C
# header only. test/run_editor_bench.sh builds and runs it.
add_executable(mdaw-editor-bench test/editor_bench.c)
set_target_properties(mdaw-editor-bench PROPERTIES C_STANDARD 11 C_STANDARD_REQUIRED ON)
target_link_libraries(mdaw-editor-bench PRIVATE mdaw_tracktion_bridge m pthread)
target_compile_definitions(mdaw-editor-bench PRIVATE
    MDAW_TEST_SCANNER="$<TARGET_FILE:mdaw-plugin-scan>"
    MDAW_TEST_CLAP_DIR="$<TARGET_FILE_DIR:mdaw-test-clap-gui>")
add_dependencies(mdaw-editor-bench mdaw-plugin-scan mdaw-test-clap-hang mdaw-test-clap-gui)
