cmake_minimum_required(VERSION 3.25)
project(latch_windows_native LANGUAGES CXX)

add_subdirectory(msys-guard)

add_executable(latch-windows-runner latch-windows-runner.cpp)
target_compile_features(latch-windows-runner PRIVATE cxx_std_20)
target_link_libraries(latch-windows-runner PRIVATE advapi32 rpcrt4)
set_target_properties(latch-windows-runner PROPERTIES
  MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>"
  RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin"
  RUNTIME_OUTPUT_DIRECTORY_DEBUG "${CMAKE_BINARY_DIR}/bin"
  RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_BINARY_DIR}/bin"
)
if(MSVC)
  target_compile_options(latch-windows-runner PRIVATE
    /W4 /WX /permissive- /EHsc /GS /guard:cf /sdl /utf-8
    /Zc:__cplusplus /Zc:inline /Zc:preprocessor)
  target_link_options(latch-windows-runner PRIVATE
    /DYNAMICBASE /NXCOMPAT /guard:cf /CETCOMPAT /OPT:REF /OPT:ICF /Brepro
    "/MANIFESTUAC:level='asInvoker' uiAccess='false'")
endif()
