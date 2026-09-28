#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <aclapi.h>

#include <cstdio>
#include <string>

#if defined(LATCH_PROBE_DETOURS)
#include "detours.h"
#endif

int wmain() {
#if defined(LATCH_PROBE_USER32)
  std::fprintf(stderr, "USER32/GDI probe: entered wmain\n");
  if (GetEnvironmentVariableW(L"LATCH_PROBE_STATION", nullptr, 0)) {
    wchar_t name[80]{};
    std::swprintf(name, 80, L"LatchControl-%lu", GetCurrentProcessId());
    HWINSTA station = CreateWindowStationW(name, 0,
        WINSTA_ENUMDESKTOPS | WINSTA_READATTRIBUTES | WINSTA_CREATEDESKTOP, nullptr);
    if (station == nullptr) {
      std::fprintf(stderr, "control CreateWindowStationW: Win32 %lu\n", GetLastError());
      return 21;
    }
    CloseWindowStation(station);
    std::fprintf(stderr, "control CreateWindowStationW: success\n");
  }
  HDC dc = CreateCompatibleDC(nullptr);
  if (dc == nullptr) return 20;
  DeleteDC(dc);
  (void)GetProcessWindowStation();
#endif
#if defined(LATCH_PROBE_WIN32)
  HANDLE token = nullptr;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) return 10;
  CloseHandle(token);
  if (GetFileAttributesW(L"C:\\Windows\\System32\\cmd.exe") == INVALID_FILE_ATTRIBUTES)
    return 11;
#endif
#if defined(LATCH_PROBE_LOAD_HOOK)
  wchar_t path[MAX_PATH]{};
  DWORD length = GetModuleFileNameW(nullptr, path, MAX_PATH);
  if (length == 0 || length >= MAX_PATH) return 12;
  std::wstring hook(path, length);
  hook.resize(hook.find_last_of(L"\\/") + 1);
  hook += L"msys-token-guard-hook.dll";
  HMODULE module = LoadLibraryW(hook.c_str());
  if (module == nullptr) {
    std::fwprintf(stderr, L"LoadLibraryW(hook) failed: %lu\n", GetLastError());
    return 13;
  }
  FreeLibrary(module);
#endif
  return 0;
}
