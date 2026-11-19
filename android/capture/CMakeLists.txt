# libopenrelay_capture.so: captura nativa (core C + adapter Oboe), salida de retorno
# (Oboe + SPSC) y JNI fino.
# Oboe llega como AAR Prefab desde app/build.gradle.kts (find_package CONFIG).
cmake_minimum_required(VERSION 3.22.1)
project(openrelay_capture LANGUAGES C CXX)

find_package(oboe REQUIRED CONFIG)

add_library(openrelay_capture SHARED
    src/life.c
    src/spsc.c
    src/capture-owner.c
    src/capture-owned.c
    src/backend-oboe.cpp
    src/jni-engine.c
    src/playback-oboe.cpp
    src/jni-playback.c)

target_include_directories(openrelay_capture PRIVATE include)
target_compile_options(openrelay_capture PRIVATE -Wall -Wextra -Werror)
target_link_options(openrelay_capture PRIVATE -Wl,-z,max-page-size=16384)
target_link_libraries(openrelay_capture PRIVATE oboe::oboe)
