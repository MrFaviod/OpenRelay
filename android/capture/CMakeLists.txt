# libarlcapture.so: captura nativa (core C + adapter Oboe), salida de retorno
# (Oboe + SPSC) y JNI fino.
# Oboe llega como AAR Prefab desde app/build.gradle.kts (find_package CONFIG).
cmake_minimum_required(VERSION 3.22.1)
project(arlcapture LANGUAGES C CXX)

find_package(oboe REQUIRED CONFIG)

add_library(arlcapture SHARED
    src/life.c
    src/spsc.c
    src/capture-owner.c
    src/capture-owned.c
    src/backend-oboe.cpp
    src/jni-engine.c
    src/playback-oboe.cpp
    src/jni-playback.c)

target_include_directories(arlcapture PRIVATE include)
target_compile_options(arlcapture PRIVATE -Wall -Wextra -Werror)
target_link_options(arlcapture PRIVATE -Wl,-z,max-page-size=16384)
target_link_libraries(arlcapture PRIVATE oboe::oboe)
