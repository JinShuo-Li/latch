cmake_minimum_required(VERSION 3.25)
project(latch_windows_boundary LANGUAGES CXX)

set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin")
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_BINARY_DIR}/bin")
set(DETOURS_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../msys-guard/vendor/detours")
add_library(detours STATIC
  "${DETOURS_DIR}/detours.cpp"
  "${DETOURS_DIR}/modules.cpp"
  "${DETOURS_DIR}/disasm.cpp"
  "${DETOURS_DIR}/image.cpp"
  "${DETOURS_DIR}/creatwth.cpp")
target_include_directories(detours PUBLIC "${DETOURS_DIR}")
set_target_properties(detours PROPERTIES CXX_STANDARD 17 MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")
add_library(latch-boundary-compat SHARED compat.cpp)
set_target_properties(latch-boundary-compat PROPERTIES MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")

target_link_libraries(latch-boundary-compat PRIVATE detours)
target_link_options(latch-boundary-compat PRIVATE "/EXPORT:DetourFinishHelperProcess,@1,NONAME")
add_executable(latch-boundary-probe probe.cpp)
set_target_properties(latch-boundary-probe PROPERTIES MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")
target_link_libraries(latch-boundary-probe PRIVATE detours advapi32 user32 userenv rpcrt4 ole32)
add_executable(latch-boundary-files files.cpp)
target_link_libraries(latch-boundary-files PRIVATE advapi32 ws2_32)
foreach(target IN ITEMS latch-boundary-probe latch-boundary-compat latch-boundary-files)
  target_compile_options(${target} PRIVATE /W4 /WX /permissive- /EHsc /GS /guard:cf /sdl /utf-8)
  target_link_options(${target} PRIVATE /DYNAMICBASE /NXCOMPAT /guard:cf /CETCOMPAT)
endforeach()
