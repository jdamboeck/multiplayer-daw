juce_add_console_app(mdaw-te-spike PRODUCT_NAME "mdaw-te-spike")
target_sources(mdaw-te-spike PRIVATE te_spike.cpp)
target_link_libraries(mdaw-te-spike PRIVATE mdaw_clap_format mdaw_engine_config)
