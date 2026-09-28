cmake_minimum_required(VERSION 3.25)
project(dsh_msys_token_guard LANGUAGES CXX)

set(DETOURS_DIR "${CMAKE_CURRENT_SOURCE_DIR}/vendor/detours")
add_library(detours STATIC
  "${DETOURS_DIR}/detours.cpp"
  "${DETOURS_DIR}/modules.cpp"
  "${DETOURS_DIR}/disasm.cpp"
  "${DETOURS_DIR}/image.cpp"
  "${DETOURS_DIR}/creatwth.cpp"
)
target_include_directories(detours PUBLIC "${DETOURS_DIR}")
target_compile_definitions(detours PRIVATE WIN32_LEAN_AND_MEAN _WIN32_WINNT=0x0A00)
set_target_properties(detours PROPERTIES
  MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>"
)

add_executable(msys-token-guard msys-token-guard.cpp)
add_library(msys-token-guard-hook SHARED msys-token-guard-hook.cpp)

foreach(probe IN ITEMS minimal win32 detours load-hook user32)
  add_executable(loader-probe-${probe} loader-probe.cpp)
  target_compile_features(loader-probe-${probe} PRIVATE cxx_std_20)
  target_compile_definitions(loader-probe-${probe} PRIVATE UNICODE _UNICODE)
  target_link_libraries(loader-probe-${probe} PRIVATE advapi32)
  set_target_properties(loader-probe-${probe} PROPERTIES
    MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>"
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_DEBUG "${CMAKE_BINARY_DIR}/bin")
  if(MSVC)
    target_compile_options(loader-probe-${probe} PRIVATE
      /W4 /WX /permissive- /EHsc /GS /guard:cf /sdl /utf-8
      /Zc:__cplusplus /Zc:inline /Zc:preprocessor)
    target_link_options(loader-probe-${probe} PRIVATE
      /DYNAMICBASE /NXCOMPAT /guard:cf /CETCOMPAT /OPT:REF /OPT:ICF /Brepro
      "/MANIFESTUAC:level='asInvoker' uiAccess='false'")
  endif()
endforeach()
target_compile_definitions(loader-probe-win32 PRIVATE LATCH_PROBE_WIN32)
target_compile_definitions(loader-probe-user32 PRIVATE LATCH_PROBE_USER32)
target_link_libraries(loader-probe-user32 PRIVATE user32 gdi32)
target_include_directories(loader-probe-detours PRIVATE "${DETOURS_DIR}")
target_link_libraries(loader-probe-detours PRIVATE detours)
target_compile_definitions(loader-probe-detours PRIVATE LATCH_PROBE_WIN32 LATCH_PROBE_DETOURS)
target_compile_definitions(loader-probe-load-hook PRIVATE LATCH_PROBE_LOAD_HOOK)
target_compile_definitions(loader-probe-load-hook PRIVATE LATCH_PROBE_WIN32 LATCH_PROBE_DETOURS)
target_include_directories(loader-probe-load-hook PRIVATE "${DETOURS_DIR}")
target_link_libraries(loader-probe-load-hook PRIVATE detours)
if(MSVC)
  target_link_options(loader-probe-detours PRIVATE "/WHOLEARCHIVE:detours")
  target_link_options(loader-probe-load-hook PRIVATE "/WHOLEARCHIVE:detours")
endif()

foreach(guard_probe IN ITEMS guard-tu guard-no-hook guard-no-inject)
  add_executable(loader-probe-${guard_probe} msys-token-guard.cpp)
  target_compile_features(loader-probe-${guard_probe} PRIVATE cxx_std_20)
  target_link_libraries(loader-probe-${guard_probe} PRIVATE detours advapi32)
  set_target_properties(loader-probe-${guard_probe} PROPERTIES
    MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>"
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_DEBUG "${CMAKE_BINARY_DIR}/bin")
  if(MSVC)
    target_compile_options(loader-probe-${guard_probe} PRIVATE
      /W4 /WX /permissive- /EHsc /GS /guard:cf /sdl /utf-8
      /Zc:__cplusplus /Zc:inline /Zc:preprocessor)
    target_link_options(loader-probe-${guard_probe} PRIVATE
      /DYNAMICBASE /NXCOMPAT /guard:cf /CETCOMPAT /OPT:REF /OPT:ICF /Brepro
      "/MANIFESTUAC:level='asInvoker' uiAccess='false'")
  endif()
endforeach()
target_compile_definitions(loader-probe-guard-tu PRIVATE LATCH_GUARD_ENTRY_PROBE)
target_compile_definitions(loader-probe-guard-no-hook PRIVATE LATCH_GUARD_NO_HOOK_PROBE)
target_compile_definitions(loader-probe-guard-no-inject PRIVATE LATCH_GUARD_NO_INJECT_PROBE)

foreach(target IN ITEMS msys-token-guard msys-token-guard-hook)
  target_compile_features(${target} PRIVATE cxx_std_20)
  target_compile_definitions(${target} PRIVATE UNICODE _UNICODE)
  target_link_libraries(${target} PRIVATE detours advapi32)
  set_target_properties(${target} PROPERTIES
    MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>"
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_DEBUG "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_RELWITHDEBINFO "${CMAKE_BINARY_DIR}/bin"
    RUNTIME_OUTPUT_DIRECTORY_MINSIZEREL "${CMAKE_BINARY_DIR}/bin"
  )
endforeach()

if(MSVC)
  target_compile_options(detours PRIVATE
    /W4 /WX /GS /guard:cf /sdl /utf-8 /Zc:inline /Zc:preprocessor
  )
  foreach(target IN ITEMS msys-token-guard msys-token-guard-hook)
    target_compile_options(${target} PRIVATE
      /W4 /WX /permissive- /EHsc /GS /guard:cf /sdl /utf-8
      /Zc:__cplusplus /Zc:inline /Zc:preprocessor
    )
    target_link_options(${target} PRIVATE
      /DYNAMICBASE /NXCOMPAT /guard:cf /CETCOMPAT /OPT:REF /OPT:ICF /Brepro
    )
  endforeach()
  target_link_options(msys-token-guard PRIVATE
    "/MANIFESTUAC:level='asInvoker' uiAccess='false'"
  )
  target_link_options(msys-token-guard-hook PRIVATE
    "/EXPORT:DetourFinishHelperProcess,@1,NONAME"
  )
else()
  foreach(target IN ITEMS detours msys-token-guard msys-token-guard-hook)
    target_compile_options(${target} PRIVATE -Wall -Wextra -Wpedantic -Werror)
  endforeach()
endif()
