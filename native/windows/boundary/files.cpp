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
  if (operation == L"root-stat") {
    for (int i = 2; i < argc; ++i) {
      WIN32_FILE_ATTRIBUTE_DATA data{};
      const BOOL stat = GetFileAttributesExW(argv[i], GetFileExInfoStandard, &data);
      const DWORD stat_error = GetLastError();
      const DWORD attributes = GetFileAttributesW(argv[i]);
      const DWORD attributes_error = GetLastError();
      HANDLE handle = CreateFileW(argv[i], FILE_READ_ATTRIBUTES,
          FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
          OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
      const DWORD open_error = GetLastError();
      std::fwprintf(stdout, L"root-stat %ls ex=%d/%lu attrs=%lu/%lu open=%d/%lu\n",
          argv[i], stat, stat_error, attributes, attributes_error,
          handle != INVALID_HANDLE_VALUE, open_error);
      if (handle != INVALID_HANDLE_VALUE) CloseHandle(handle);
    }
    return 0;
  }
  if (operation == L"root-boundary") {
    if (argc != 3) return 2;
    HANDLE root = CreateFileW(argv[2], FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (root == INVALID_HANDLE_VALUE) return 3;
    FILE_ID_INFO id{};
    const bool metadata = GetFileInformationByHandleEx(root, FileIdInfo, &id, sizeof(id)) != FALSE;
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    const DWORD acl = GetSecurityInfo(root, SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION, nullptr, nullptr, nullptr, nullptr, &descriptor);
    if (descriptor) LocalFree(descriptor);
    CloseHandle(root);
    if (!metadata || acl != ERROR_ACCESS_DENIED) return 4;
    std::puts("PASS root ancestor handle has metadata rights, no ACL read");
    return 0;
  }
  if (operation == L"ancestor-metadata") {
    if (argc != 3) return 2;
    const std::wstring trailing = std::wstring(argv[2]) + L"\\";
    HANDLE directory = CreateFileW(trailing.c_str(),
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (directory == INVALID_HANDLE_VALUE) {
      std::fwprintf(stderr, L"FAIL ancestor metadata open %ls error=%lu\n",
                    trailing.c_str(), GetLastError());
      return 3;
    }
    FILE_ID_INFO id{};
    const bool metadata = GetFileInformationByHandleEx(directory, FileIdInfo,
                                                        &id, sizeof(id)) != FALSE;
    alignas(FILE_ID_BOTH_DIR_INFO) BYTE listing[4096]{};
    const bool enumerated = GetFileInformationByHandleEx(directory, FileIdBothDirectoryInfo,
                                                          listing, sizeof(listing)) != FALSE;
    const DWORD list_error = GetLastError();
    CloseHandle(directory);
    if (!metadata || enumerated || list_error != ERROR_ACCESS_DENIED) {
      std::fwprintf(stderr, L"FAIL ancestor metadata=%d enumerate=%d/%lu\n",
                    metadata, enumerated, list_error);
      return 4;
    }
    std::puts("PASS ancestor has metadata only, even with trailing separator");
    return 0;
  }
  if (operation == L"enumerate-allow" || operation == L"enumerate-deny") {
    if (argc != 3) return 2;
    HANDLE directory = CreateFileW(argv[2], FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    const DWORD open_error = GetLastError();
    if (directory != INVALID_HANDLE_VALUE) CloseHandle(directory);
    const std::wstring pattern = std::wstring(argv[2]) + L"\\*";
    WIN32_FIND_DATAW entry{};
    HANDLE listing = FindFirstFileW(pattern.c_str(), &entry);
    const DWORD list_error = GetLastError();
    if (listing != INVALID_HANDLE_VALUE) FindClose(listing);
    const bool expected = operation == L"enumerate-allow";
    const bool opened = directory != INVALID_HANDLE_VALUE;
    const bool listed = listing != INVALID_HANDLE_VALUE;
    if ((expected && !opened) || listed != expected ||
        (!expected && list_error != ERROR_ACCESS_DENIED)) {
      std::fwprintf(stderr, L"FAIL %ls %ls open=%d/%lu list=%d/%lu\n",
                    argv[1], argv[2], opened, open_error, listed, list_error);
      return 1;
    }
    std::fwprintf(stdout, L"PASS %ls\n", argv[1]);
    return 0;
  }
  if (operation == L"cwd-probe") {
    wchar_t cwd[32768]{};
    if (!GetCurrentDirectoryW(32768, cwd)) return 3;
    for (DWORD share : {static_cast<DWORD>(FILE_SHARE_READ | FILE_SHARE_WRITE),
                        static_cast<DWORD>(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)}) {
      HANDLE opened = CreateFileW(cwd, 0, share, nullptr, OPEN_EXISTING,
                                  FILE_FLAG_BACKUP_SEMANTICS, nullptr);
      const DWORD error = GetLastError();
      std::fwprintf(stdout, L"cwd share=%lu opened=%d error=%lu path=%ls\n",
                    share, opened != INVALID_HANDLE_VALUE, error, cwd);
      if (opened != INVALID_HANDLE_VALUE) {
        wchar_t resolved[32768]{};
        for (DWORD flags : {static_cast<DWORD>(0),
                            static_cast<DWORD>(FILE_NAME_OPENED),
                            static_cast<DWORD>(FILE_NAME_OPENED | VOLUME_NAME_NONE)}) {
          const DWORD length = GetFinalPathNameByHandleW(opened, resolved, 32768, flags);
          std::fwprintf(stdout, L"cwd final flags=%lu length=%lu error=%lu value=%ls\n",
                        flags, length, GetLastError(), resolved);
        }
        CloseHandle(opened);
      }
    }
    return 0;
  }
  if (operation == L"replace-file") {
    const std::wstring staging = std::wstring(argv[2]) + L".replacement";
    HANDLE file = CreateFileW(staging.c_str(), GENERIC_WRITE, 0, nullptr,
        CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) return 3;
    constexpr char content[] = "replacement contents";
    DWORD written = 0;
    const BOOL stored = WriteFile(file, content, sizeof(content) - 1, &written, nullptr);
    CloseHandle(file);
    if (!stored || written != sizeof(content) - 1) return 4;
    if (!MoveFileExW(staging.c_str(), argv[2],
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) return 5;
    std::puts("PASS existing file replacement");
    return 0;
  }
  if (operation == L"delete-file") {
    if (!DeleteFileW(argv[2])) return 3;
    std::puts("PASS existing file deletion");
    return 0;
  }
  if (operation == L"rename-file") {
    if (argc != 4 || !MoveFileExW(argv[2], argv[3], MOVEFILE_WRITE_THROUGH)) return 3;
    std::puts("PASS existing file rename");
    return 0;
  }
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
    if (generation == 3) {
      HANDLE token = nullptr;
      if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) return 7;
      DWORD size = 0;
      GetTokenInformation(token, TokenAppContainerSid, nullptr, 0, &size);
      std::vector<BYTE> bytes(size);
      const BOOL queried = GetTokenInformation(token, TokenAppContainerSid, bytes.data(), size, &size);
      CloseHandle(token);
      if (!queried) return 7;
      const auto* info = reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(bytes.data());
      LPWSTR sid = nullptr;
      if (!info->TokenAppContainer || !ConvertSidToStringSidW(info->TokenAppContainer, &sid)) return 7;
      const std::wstring wide_sid(sid);
      LocalFree(sid);
      std::string package;
      for (const wchar_t character : wide_sid) package.push_back(static_cast<char>(character));
      const auto package_file = std::wstring(argv[3]) + L".package";
      HANDLE output = CreateFileW(package_file.c_str(), GENERIC_WRITE, FILE_SHARE_READ,
          nullptr, CREATE_NEW, 0, nullptr);
      if (output == INVALID_HANDLE_VALUE) return 7;
      DWORD written = 0;
      const BOOL stored = WriteFile(output, package.data(), static_cast<DWORD>(package.size()),
                                    &written, nullptr);
      CloseHandle(output);
      if (!stored || written != package.size()) return 7;
    }
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
  else if (operation == L"write-allow" || operation == L"write-deny" ||
           operation == L"junction-write-deny") access = GENERIC_WRITE;
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
  // AppContainer path traversal can report PATH_NOT_FOUND for a junction whose
  // target is outside its namespace. The host fixture checks the target before
  // and after this attempt; successful opening is always a security failure.
  const bool junction_hidden = operation == L"junction-write-deny" &&
                               (error == ERROR_PATH_NOT_FOUND ||
                                error == ERROR_FILE_NOT_FOUND);
  if (opened != expected || (!opened && error != ERROR_ACCESS_DENIED &&
                             !junction_hidden)) {
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
