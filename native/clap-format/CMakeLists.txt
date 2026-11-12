# A CLAP plugin format for JUCE's plugin host, so that Tracktion Engine can load CLAP plugins the
# same way it loads VST3 ones. JUCE 8.0.6 and Tracktion Engine 3.2 host no CLAP themselves.
add_library(mdaw_clap_format STATIC clap_format.cpp x11_atoms.cpp)
target_include_directories(mdaw_clap_format
    PUBLIC  ${CMAKE_CURRENT_SOURCE_DIR}
    PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/clap/include)
target_link_libraries(mdaw_clap_format PUBLIC mdaw_engine_config juce::juce_gui_extra ${CMAKE_DL_LIBS})
