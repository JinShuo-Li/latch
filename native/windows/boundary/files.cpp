// Native subprocess assertions. No shell policy parsing is involved.
#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <aclapi.h>
#include <sddl.h>
#include <vector>
#include <cstdio>
#include <cwchar>
#include <string>

int wmain(int argc, wchar_t** argv) {
  if (argc < 3) return 2;
  const std::wstring operation = argv[1];
  if (operation == L"network-deny" || operation == L"network-allow") {
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data)) return 3;
    SOCKET socket = ::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    int error = WSAGetLastError();
    bool connected = false;
    if (socket != INVALID_SOCKET) {
      sockaddr_in target{};
      target.sin_family = AF_INET;
      target.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      if (argc == 4 && InetPtonW(AF_INET, argv[3], &target.sin_addr) != 1) return 4;
      target.sin_port = htons(static_cast<u_short>(_wtoi(argv[2])));
      u_long nonblocking = 1;
      if (ioctlsocket(socket, FIONBIO, &nonblocking)) return 5;
      connected = connect(socket, reinterpret_cast<sockaddr*>(&target), sizeof(target)) == 0;
      error = WSAGetLastError();
      if (!connected && error == WSAEWOULDBLOCK) {
        fd_set writable, errors;
        FD_ZERO(&writable); FD_ZERO(&errors);
        FD_SET(socket, &writable); FD_SET(socket, &errors);
        timeval timeout{2, 0};
        const int ready = select(0, nullptr, &writable, &errors, &timeout);
        if (ready > 0) {
          int size = sizeof(error);
          if (getsockopt(socket, SOL_SOCKET, SO_ERROR, reinterpret_cast<char*>(&error), &size)) return 6;
          connected = error == 0;
        } else error = ready == 0 ? WSAETIMEDOUT : WSAGetLastError();
      }
      closesocket(socket);
    }
    WSACleanup();
    std::printf("network connected=%d error=%d\n", connected, error);
    return operation == L"network-allow" ? (connected ? 0 : 1) :
        (!connected && (error == WSAEACCES || error == WSAETIMEDOUT) ? 0 : 1);
  }
  if (operation == L"privilege-deny") {
    HANDLE token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES, &token)) return 3;
    TOKEN_PRIVILEGES privilege{};
    privilege.PrivilegeCount = 1;
    if (!LookupPrivilegeValueW(nullptr, SE_DEBUG_NAME, &privilege.Privileges[0].Luid)) return 4;
    privilege.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    SetLastError(ERROR_SUCCESS);
    const BOOL changed = AdjustTokenPrivileges(token, FALSE, &privilege, 0, nullptr, nullptr);
    const DWORD error = GetLastError();
    CloseHandle(token);
    if (changed && error != ERROR_NOT_ALL_ASSIGNED) return 1;
    HKEY registry = nullptr;
    const LSTATUS opened = RegOpenKeyExW(HKEY_LOCAL_MACHINE, L"SYSTEM", 0, KEY_SET_VALUE, &registry);
    if (opened == ERROR_SUCCESS) { RegCloseKey(registry); return 1; }
    if (opened != ERROR_ACCESS_DENIED) return 2;
    std::puts("PASS debug privilege and system registry write denied");
    return 0;
  }
  if (operation == L"mkdir-deny") {
    const BOOL created = CreateDirectoryW(argv[2], nullptr);
    const DWORD error = GetLastError();
    if (created || (error != ERROR_ACCESS_DENIED && error != ERROR_ALREADY_EXISTS)) return 1;
    std::puts("PASS mkdir-deny");
    return 0;
  }
  if (operation == L"inspect-access") {
    HANDLE token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) return 3;
    for (auto type : {TokenGroups, TokenRestrictedSids, TokenCapabilities}) {
      DWORD size = 0;
      GetTokenInformation(token, type, nullptr, 0, &size);
      std::vector<BYTE> data(size);
      if (!GetTokenInformation(token, type, data.data(), size, &size)) return 4;
      auto* groups = reinterpret_cast<TOKEN_GROUPS*>(data.data());
      for (DWORD i = 0; i < groups->GroupCount; ++i) {
        LPWSTR sid = nullptr;
        ConvertSidToStringSidW(groups->Groups[i].Sid, &sid);
        std::fwprintf(stdout, L"type=%d attributes=%lx sid=%ls", type, groups->Groups[i].Attributes, sid);
        std::fputwc(10, stdout);
        LocalFree(sid);
      }
    }
    CloseHandle(token);
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    const DWORD error = GetNamedSecurityInfoW(argv[2], SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
        nullptr, nullptr, nullptr, nullptr, &descriptor);
    std::fwprintf(stdout, L"descriptor error=%lu ", error);
    if (error == ERROR_SUCCESS) {
      LPWSTR sddl = nullptr;
      ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1,
          DACL_SECURITY_INFORMATION, &sddl, nullptr);
      std::fwprintf(stdout, L"%ls", sddl);
      LocalFree(sddl);
      LocalFree(descriptor);
    }
    std::fputwc(10, stdout);
    for (DWORD access : {static_cast<DWORD>(FILE_READ_DATA), static_cast<DWORD>(GENERIC_READ)}) {
      HANDLE file = CreateFileW(argv[2], access, FILE_SHARE_READ | FILE_SHARE_WRITE,
          nullptr, OPEN_EXISTING, 0, nullptr);
      std::fwprintf(stdout, L"access=%lx opened=%d error=%lu", access, file != INVALID_HANDLE_VALUE, GetLastError());
      std::fputwc(10, stdout);
      if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    }
    return 0;
  }
  if (operation == L"make-null-dacl") {
    SECURITY_DESCRIPTOR descriptor{};
    if (!InitializeSecurityDescriptor(&descriptor, SECURITY_DESCRIPTOR_REVISION) ||
        !SetSecurityDescriptorDacl(&descriptor, TRUE, nullptr, FALSE) ||
        !SetFileSecurityW(argv[2], DACL_SECURITY_INFORMATION |
            PROTECTED_DACL_SECURITY_INFORMATION, &descriptor)) return 3;
    return 0;
  }
  if (operation == L"tree" || operation == L"tree-root-exit") {
    const int generation = _wtoi(argv[2]);
    if (argc != 4 || generation < 0 || generation > 3) return 2;
    const std::wstring file = std::wstring(argv[3]) + L"." + std::to_wstring(generation);
    HANDLE marker = CreateFileW(file.c_str(), GENERIC_WRITE, FILE_SHARE_READ,
                               nullptr, CREATE_ALWAYS, 0, nullptr);
    if (marker == INVALID_HANDLE_VALUE) return 3;
    const DWORD pid = GetCurrentProcessId();
    DWORD written = 0;
    const bool stored = WriteFile(marker, &pid, sizeof(pid), &written, nullptr) != FALSE;
    CloseHandle(marker);
    if (!stored || written != sizeof(pid)) return 4;
    if (generation > 0) {
      wchar_t executable[32768];
      if (!GetModuleFileNameW(nullptr, executable, 32768)) return 5;
      std::wstring line = L"\"" + std::wstring(executable) + L"\" tree " +
          std::to_wstring(generation - 1) + L" \"" + argv[3] + L"\"";
      STARTUPINFOW startup{};
      startup.cb = sizeof(startup);
      PROCESS_INFORMATION child{};
      if (!CreateProcessW(executable, line.data(), nullptr, nullptr, FALSE,
                          CREATE_NO_WINDOW, nullptr, nullptr, &startup, &child)) return 6;
      CloseHandle(child.hThread);
      CloseHandle(child.hProcess);
    }
    if (operation == L"tree-root-exit") { Sleep(750); return 0; }
    Sleep(10'000);
    return 0;
  }
  if (argc != 3) return 2;
  DWORD access = 0;
  DWORD disposition = OPEN_EXISTING;
  if (operation == L"read-allow" || operation == L"read-deny") access = GENERIC_READ;
  else if (operation == L"write-allow" || operation == L"write-deny") access = GENERIC_WRITE;
  else if (operation == L"acl-deny") access = WRITE_DAC | WRITE_OWNER;
  else if (operation == L"dacl-deny") access = WRITE_DAC;
  else if (operation == L"owner-deny") access = WRITE_OWNER;
  else if (operation == L"delete-deny") access = DELETE;
  else if (operation == L"create-allow" || operation == L"create-deny") {
    access = GENERIC_WRITE;
    disposition = CREATE_NEW;
  } else return 2;
  const bool expected = operation.ends_with(L"-allow");
  HANDLE file = CreateFileW(argv[2], access, FILE_SHARE_READ | FILE_SHARE_WRITE,
                            nullptr, disposition, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
  const DWORD error = GetLastError();
  const bool opened = file != INVALID_HANDLE_VALUE;
  if (opened) CloseHandle(file);
  if (opened != expected || (!opened && error != ERROR_ACCESS_DENIED)) {
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    if (GetNamedSecurityInfoW(argv[2], SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
        nullptr, nullptr, nullptr, nullptr, &descriptor) == ERROR_SUCCESS) {
      LPWSTR sddl = nullptr;
      if (ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1,
          DACL_SECURITY_INFORMATION, &sddl, nullptr)) {
        std::fwprintf(stderr, L"DACL at failed access: %ls", sddl);
        std::fputwc(10, stderr);
        LocalFree(sddl);
      }
      LocalFree(descriptor);
    }
    std::fwprintf(stderr, L"FAIL %ls %ls opened=%d Win32=%lu\n",
                  argv[1], argv[2], opened, error);
    return 1;
  }
  std::fwprintf(stdout, L"PASS %ls\n", argv[1]);
  return 0;
}
