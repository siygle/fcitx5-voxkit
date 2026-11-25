# Optional translation plugin for the voxkit daemon.
#
#   cmake -S daemon/mt-plugin -B build-mt -DLLAMA_CPP_SOURCE_DIR=/path/to/llama.cpp
#   cmake --build build-mt
#   -> build-mt/libvoxkit-mt.so (Linux) / libvoxkit-mt.dylib (macOS)
#
# llama.cpp and its ggml are compiled statically (PIC) INTO the plugin with
# hidden visibility; only the vk_mt_* C functions are exported. The daemon
# links transcribe-cpp's own ggml 0.26; the two never see each other.
# The llama.cpp revision is pinned in LLAMA_PIN (tag and commit).
# Without LLAMA_CPP_SOURCE_DIR, -DVOXKIT_MT_FETCH=ON downloads that pin.
cmake_minimum_required(VERSION 3.16)
project(voxkit-mt LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(CMAKE_C_VISIBILITY_PRESET hidden)
set(CMAKE_CXX_VISIBILITY_PRESET hidden)
set(CMAKE_VISIBILITY_INLINES_HIDDEN ON)

file(READ "${CMAKE_CURRENT_SOURCE_DIR}/LLAMA_PIN" _pin)
string(REGEX MATCH "^([^ ]+) ([0-9a-f]+)" _ "${_pin}")
set(VOXKIT_MT_LLAMA_TAG "${CMAKE_MATCH_1}")
set(VOXKIT_MT_LLAMA_COMMIT "${CMAKE_MATCH_2}")

set(LLAMA_CPP_SOURCE_DIR "" CACHE PATH "llama.cpp checkout at the pinned commit")
option(VOXKIT_MT_FETCH "Download the pinned llama.cpp when no source dir is given" OFF)

# Static llama + ggml inside the plugin. CPU only, no OpenMP (no libgomp
# runtime dependency), no network/tools.
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(GGML_STATIC OFF CACHE BOOL "" FORCE)
set(GGML_OPENMP OFF CACHE BOOL "" FORCE)
set(GGML_BACKEND_DL OFF CACHE BOOL "" FORCE)
set(GGML_METAL OFF CACHE BOOL "" FORCE)
set(GGML_BLAS OFF CACHE BOOL "" FORCE)
set(GGML_CCACHE OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_COMMON OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TOOLS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_SERVER OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_APP OFF CACHE BOOL "" FORCE)
set(LLAMA_CURL OFF CACHE BOOL "" FORCE)
set(LLAMA_OPENSSL OFF CACHE BOOL "" FORCE)
set(LLAMA_ALL_WARNINGS OFF CACHE BOOL "" FORCE)
# CPU baseline: GGML_NATIVE etc. come from the caller (the release build
# passes the same x86-64-v3 flags it uses for transcribe-cpp).

if (LLAMA_CPP_SOURCE_DIR)
    add_subdirectory("${LLAMA_CPP_SOURCE_DIR}" llama.cpp EXCLUDE_FROM_ALL)
elseif (VOXKIT_MT_FETCH)
    include(FetchContent)
    FetchContent_Declare(llama_cpp
        GIT_REPOSITORY https://github.com/ggml-org/llama.cpp.git
        GIT_TAG ${VOXKIT_MT_LLAMA_COMMIT})
    FetchContent_GetProperties(llama_cpp)
    if (NOT llama_cpp_POPULATED)
        FetchContent_Populate(llama_cpp)
    endif()
    add_subdirectory("${llama_cpp_SOURCE_DIR}" llama.cpp EXCLUDE_FROM_ALL)
else()
    message(FATAL_ERROR "set LLAMA_CPP_SOURCE_DIR (llama.cpp ${VOXKIT_MT_LLAMA_TAG} "
                        "${VOXKIT_MT_LLAMA_COMMIT}) or -DVOXKIT_MT_FETCH=ON")
endif()

add_library(voxkit-mt SHARED voxkit_mt.cpp)
target_link_libraries(voxkit-mt PRIVATE llama)
target_compile_definitions(voxkit-mt PRIVATE VOXKIT_MT_LLAMA_TAG="${VOXKIT_MT_LLAMA_TAG}")
set_target_properties(voxkit-mt PROPERTIES OUTPUT_NAME voxkit-mt)
if (APPLE)
    target_link_options(voxkit-mt PRIVATE
        "-Wl,-exported_symbols_list,${CMAKE_CURRENT_SOURCE_DIR}/voxkit_mt.exp")
else()
    target_link_options(voxkit-mt PRIVATE
        "-Wl,--version-script=${CMAKE_CURRENT_SOURCE_DIR}/voxkit_mt.map"
        "-Wl,--exclude-libs,ALL" "-Wl,--no-undefined")
endif()

include(GNUInstallDirs)
install(TARGETS voxkit-mt LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}/voxkit")
