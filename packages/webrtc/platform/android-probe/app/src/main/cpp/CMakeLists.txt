cmake_minimum_required(VERSION 3.22)
project(lingxi_webrtc_device_probe LANGUAGES CXX)
get_filename_component(WEBRTC_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../../../../../.." ABSOLUTE)
add_library(lingxi_webrtc_mobile STATIC IMPORTED)
set_target_properties(lingxi_webrtc_mobile PROPERTIES IMPORTED_LOCATION "${WEBRTC_ROOT}/target/aarch64-linux-android/debug/liblingxi_webrtc_mobile.a")
add_library(lingxi_webrtc_probe SHARED bridge.cpp)
# The debug fixture renders full video frames; keep native pixel conversion
# optimized just like the Rust media library while retaining debug symbols.
target_compile_options(lingxi_webrtc_probe PRIVATE -O2)
target_include_directories(lingxi_webrtc_probe PRIVATE "${WEBRTC_ROOT}/mobile/include")
target_link_libraries(lingxi_webrtc_probe PRIVATE lingxi_webrtc_mobile android jnigraphics log m z dl)
target_link_options(lingxi_webrtc_probe PRIVATE "-Wl,-z,max-page-size=16384")
