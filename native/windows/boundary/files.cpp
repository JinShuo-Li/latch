// Native subprocess assertions. No shell policy parsing is involved.
#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <winternl.h>
#include "detours.h"
#include "handles.h"
#include "broker_protocol.h"
#include <aclapi.h>
#include <sddl.h>
#include <vector>
#include <cstdio>
#include <cwchar>
#include <string>
#include <io.h>
#include <cerrno>

int wmain(int argc, wchar_t** argv) {
  if (argc < 3) return 2;
  const std::wstring operation = argv[1];
  if (operation == L"raw-read-deny") {
    using Factory = decltype(&NtCreateFile)(*)();
    const auto factory = reinterpret_cast<Factory>(GetProcAddress(GetModuleHandleW(L"latch-boundary-compat.dll"), "LatchTestCreateFile"));
    if (!factory) return 2;
    std::wstring name = L"\\??\\" + std::wstring(argv[2]);
    UNICODE_STRING text{static_cast<USHORT>(name.size() * sizeof(wchar_t)),
                        static_cast<USHORT>(name.size() * sizeof(wchar_t)), name.data()};
    OBJECT_ATTRIBUTES attributes{sizeof(attributes), nullptr, &text, OBJ_CASE_INSENSITIVE, nullptr, nullptr};
    IO_STATUS_BLOCK io{};
    HANDLE file = nullptr;
    const auto status = factory()(&file, FILE_GENERIC_READ, &attributes, &io, nullptr, 0,
        FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
    if (status >= 0) CloseHandle(file);
    return status == static_cast<NTSTATUS>(0xc0000022u) ? 0 : 3;
  }
  if (operation == L"create-existing") {
    HANDLE file = CreateFileW(argv[2], GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, CREATE_NEW, 0, nullptr);
    const DWORD error = GetLastError();
    if (file != INVALID_HANDLE_VALUE) { CloseHandle(file); return 1; }
    return error == ERROR_FILE_EXISTS ? 0 : 2;
  }
  if (operation == L"raw-directory-write-deny") {
    HANDLE directory = CreateFileW(argv[2], GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (directory != INVALID_HANDLE_VALUE) { CloseHandle(directory); return 1; }
    return GetLastError() == ERROR_ACCESS_DENIED ? 0 : 2;
  }
  if (operation == L"raw-namespace-deny") {
    HANDLE directory = CreateFileW(argv[2], GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (directory == INVALID_HANDLE_VALUE) return 2;
    using Factory = decltype(&NtCreateFile)(*)();
    const auto factory = reinterpret_cast<Factory>(GetProcAddress(GetModuleHandleW(L"latch-boundary-compat.dll"), "LatchTestCreateFile"));
    if (!factory) { CloseHandle(directory); return 3; }
    wchar_t name[] = L"raw-denied.txt";
    UNICODE_STRING text{sizeof(name) - sizeof(wchar_t), sizeof(name), name};
    OBJECT_ATTRIBUTES attributes{sizeof(attributes), directory, &text, OBJ_CASE_INSENSITIVE, nullptr, nullptr};
    IO_STATUS_BLOCK io{};
    HANDLE file = nullptr;
    const NTSTATUS status = factory()(&file, GENERIC_WRITE | SYNCHRONIZE, &attributes,
        &io, nullptr, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
    if (status >= 0) CloseHandle(file);
    CloseHandle(directory);
    return status == static_cast<NTSTATUS>(0xc0000022u) ? 0 : 4;
  }
  if (operation == L"raw-rename-outside-deny" || operation == L"raw-link-outside-deny") {
    if (argc != 4) return 2;
    HANDLE source = CreateFileW(argv[2], DELETE | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING, 0, nullptr);
    HANDLE directory = CreateFileW(argv[3], FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (source == INVALID_HANDLE_VALUE || directory == INVALID_HANDLE_VALUE) return 3;
    using Set = NTSTATUS(NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
    using Factory = Set(*)();
    const auto factory = reinterpret_cast<Factory>(GetProcAddress(GetModuleHandleW(L"latch-boundary-compat.dll"), "LatchTestSetInformation"));
    if (!factory) return 4;
    const std::wstring name = L"raw-escape.txt";
    std::vector<BYTE> buffer(offsetof(FILE_RENAME_INFO, FileName) + name.size() * sizeof(wchar_t));
    auto* rename = reinterpret_cast<FILE_RENAME_INFO*>(buffer.data());
    rename->RootDirectory = directory;
    rename->FileNameLength = static_cast<DWORD>(name.size() * sizeof(wchar_t));
    memcpy(rename->FileName, name.data(), rename->FileNameLength);
    IO_STATUS_BLOCK io{};
    const auto kind = static_cast<FILE_INFORMATION_CLASS>(operation == L"raw-rename-outside-deny" ? 10 : 11);
    const NTSTATUS status = factory()(source, &io, rename, static_cast<ULONG>(buffer.size()), kind);
    CloseHandle(source); CloseHandle(directory);
    return status == static_cast<NTSTATUS>(0xc0000022u) ? 0 : 5;
  }
  if (operation == L"raw-dacl-deny") {
    HANDLE file = CreateFileW(argv[2], GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_EXISTING, 0, nullptr);
    if (file == INVALID_HANDLE_VALUE) return 2;
    SECURITY_DESCRIPTOR descriptor{};
    InitializeSecurityDescriptor(&descriptor, SECURITY_DESCRIPTOR_REVISION);
    SetSecurityDescriptorDacl(&descriptor, TRUE, nullptr, FALSE);
    const BOOL changed = SetKernelObjectSecurity(file, DACL_SECURITY_INFORMATION, &descriptor);
    const DWORD error = GetLastError();
    CloseHandle(file);
    return !changed && error == ERROR_ACCESS_DENIED ? 0 : 3;
  }
  if (operation == L"crt-read") {
    auto legacy = LoadLibraryW(L"msvcrt.dll");
    using Access = int (__cdecl*)(const wchar_t*, int);
    const auto legacy_access = reinterpret_cast<Access>(GetProcAddress(legacy, "_waccess"));
    const int legacy_result = legacy_access(argv[2], 4);
    std::fwprintf(stderr, L"legacy access result=%d win=%lu\n", legacy_result, GetLastError());
    const int result = _waccess(argv[2], 4);
    std::fwprintf(stderr, L"access result=%d errno=%d win=%lu\n", result, errno, GetLastError());
    FILE* file = nullptr;
    _wfopen_s(&file, argv[2], L"rb");
    std::fwprintf(stderr, L"fopen=%d errno=%d win=%lu\n", file != nullptr, errno, GetLastError());
    if (!file) return 1;
    const int first = std::fgetc(file);
    std::fwprintf(stderr, L"first=%d error=%d errno=%d win=%lu\n", first, std::ferror(file), errno, GetLastError());
    std::fclose(file);
    return result == 0 && first != EOF ? 0 : 1;
  }
  if (operation == L"broker-read-allow" || operation == L"broker-read-deny" ||
      operation == L"broker-access-deny" || operation == L"broker-caller-deny" ||
      operation == L"broker-socket-deny" || operation == L"broker-raw-deny" ||
      operation == L"broker-socket-caller-deny") {
    DWORD size = 0;
    const auto* broker = static_cast<const LatchHandles*>(
        DetourFindPayloadEx(latch_handles_id, &size));
    if (!broker || size != sizeof(LatchHandles) || !broker->read_broker ||
        !broker->broker_mutex || wcslen(argv[2]) >= 32768) return 3;
    const DWORD waited = WaitForSingleObject(broker->broker_mutex, 5000);
    if (waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED) return 4;
    LatchReadRequest request{};
    request.version = latch_broker_version;
    request.process_id = GetCurrentProcessId();
    request.request_id = 0x42524f4b4552;
    request.access = operation == L"broker-access-deny" ? GENERIC_ALL : GENERIC_READ;
    request.share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    request.options = 0x20;  // FILE_SYNCHRONOUS_IO_NONALERT
    request.path_length = static_cast<DWORD>(wcslen(argv[2]));
    memcpy(request.path, argv[2], request.path_length * sizeof(wchar_t));
    if (operation == L"broker-socket-deny" || operation == L"broker-raw-deny" ||
        operation == L"broker-socket-caller-deny") {
      request.operation = LatchBrokerOperation::socket_create;
      request.family = AF_INET;
      request.socket_type = operation == L"broker-raw-deny" ? SOCK_RAW : SOCK_STREAM;
      request.protocol = IPPROTO_TCP;
      request.socket_flags = WSA_FLAG_OVERLAPPED;
    }
    if ((operation == L"broker-caller-deny" || operation == L"broker-socket-caller-deny") &&
        !GetNamedPipeServerProcessId(broker->read_broker, &request.process_id)) {
      ReleaseMutex(broker->broker_mutex);
      return 5;
    }
    DWORD bytes = 0;
    LatchReadResponse response{};
    const bool exchanged = WriteFile(broker->read_broker, &request,
        latch_read_request_size(request.path_length), &bytes, nullptr) &&
        bytes == latch_read_request_size(request.path_length) &&
        ReadFile(broker->read_broker, &response, sizeof(response), &bytes, nullptr) &&
        bytes == sizeof(response) && response.request_id == request.request_id;
    ReleaseMutex(broker->broker_mutex);
    const bool allowed = exchanged && !response.error && (response.file || response.socket_ticket);
    if (allowed && response.file) CloseHandle(response.file);
    if (!exchanged || allowed != (operation == L"broker-read-allow")) {
      std::fwprintf(stderr, L"Direct broker exchanged=%d allowed=%d error=%lu version=%lu id=%llu bytes=%lu win=%lu\n",
        exchanged, allowed, response.error, response.version, response.request_id, bytes, GetLastError());
      return 6;
    }
    std::fwprintf(stdout, L"PASS direct %ls\n", operation.c_str());
    return 0;
  }
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
  if (operation == L"network-resolve") {
    if (argc != 3) return 2;
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data)) return 3;
    ADDRINFOW hints{};
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    ADDRINFOW* addresses = nullptr;
    const int error = GetAddrInfoW(argv[2], nullptr, &hints, &addresses);
    if (error) {
      WSACleanup();
      std::fwprintf(stderr, L"FAIL resolve %ls error=%d\n", argv[2], error);
      return 1;
    }
    unsigned count = 0;
    for (const auto* address = addresses; address;
         address = address->ai_next)
      ++count;
    FreeAddrInfoW(addresses);
    WSACleanup();
    std::fwprintf(stdout, L"resolved host=%ls addresses=%u\n", argv[2], count);
    return count ? 0 : 1;
  }
  if (operation == L"network-deny" || operation == L"network-allow" ||
      operation == L"network-echo") {
    if (argc != 3 && argc != 4) return 2;
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data)) return 3;
    const wchar_t* host = argc == 4 ? argv[3] : L"127.0.0.1";
    ADDRINFOW hints{};
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    ADDRINFOW* addresses = nullptr;
    const int resolve_error = GetAddrInfoW(host, argv[2], &hints, &addresses);
    if (resolve_error) {
      WSACleanup();
      std::fwprintf(stderr, L"FAIL resolve %ls error=%d\n", host,
                    resolve_error);
      return 7;
    }
    int error = WSAENETUNREACH;
    bool connected = false;
    for (const auto* address = addresses; address;
         address = address->ai_next) {
      SOCKET socket = ::socket(address->ai_family, address->ai_socktype,
                               address->ai_protocol);
      if (socket == INVALID_SOCKET) {
        error = WSAGetLastError();
        continue;
      }
      u_long nonblocking = 1;
      if (ioctlsocket(socket, FIONBIO, &nonblocking)) {
        error = WSAGetLastError();
        closesocket(socket);
        continue;
      }
      connected = connect(socket, address->ai_addr,
                          static_cast<int>(address->ai_addrlen)) == 0;
      error = connected ? 0 : WSAGetLastError();
      if (!connected && error == WSAEWOULDBLOCK) {
        fd_set writable, errors;
        FD_ZERO(&writable); FD_ZERO(&errors);
        FD_SET(socket, &writable); FD_SET(socket, &errors);
        timeval timeout{2, 0};
        const int ready = select(0, nullptr, &writable, &errors, &timeout);
        if (ready > 0) {
          int size = sizeof(error);
          if (getsockopt(socket, SOL_SOCKET, SO_ERROR,
                         reinterpret_cast<char*>(&error), &size))
            error = WSAGetLastError();
          connected = error == 0;
        } else error = ready == 0 ? WSAETIMEDOUT : WSAGetLastError();
      }
      if (connected && operation == L"network-echo") {
        u_long blocking = 0;
        DWORD timeout_ms = 3000;
        ioctlsocket(socket, FIONBIO, &blocking);
        setsockopt(socket, SOL_SOCKET, SO_RCVTIMEO,
                   reinterpret_cast<const char*>(&timeout_ms), sizeof(timeout_ms));
        const char payload[] = "latch";
        char reply[sizeof(payload) - 1]{};
        connected = send(socket, payload, sizeof(payload) - 1, 0) == sizeof(payload) - 1;
        int received = 0;
        while (connected && received < static_cast<int>(sizeof(reply))) {
          const int count = recv(socket, reply + received, sizeof(reply) - received, 0);
          if (count <= 0) { connected = false; error = WSAGetLastError(); break; }
          received += count;
        }
        connected = connected && memcmp(payload, reply, sizeof(reply)) == 0;
      }
      closesocket(socket);
      if (connected) break;
    }
    FreeAddrInfoW(addresses);
    WSACleanup();
    std::fwprintf(stdout, L"network host=%ls connected=%d error=%d\n", host,
                  connected, error);
    const bool denied =
        !connected &&
        (error == WSAEACCES || error == WSAETIMEDOUT ||
         error == WSAECONNREFUSED || error == WSAENETUNREACH ||
         error == WSAEHOSTUNREACH);
    return operation != L"network-deny" ? (connected ? 0 : 1)
                                         : (denied ? 0 : 1);
  }
  if (operation == L"network-udp-echo" || operation == L"network-udp-deny") {
    if (argc != 4) return 2;
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data)) return 3;
    ADDRINFOW hints{};
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_DGRAM;
    hints.ai_protocol = IPPROTO_UDP;
    ADDRINFOW* addresses = nullptr;
    if (GetAddrInfoW(argv[3], argv[2], &hints, &addresses)) { WSACleanup(); return 4; }
    const auto* address = addresses;
    SOCKET socket = INVALID_SOCKET;
    if (address->ai_family == AF_INET) {
      using CreateSocket = SOCKET(WSAAPI*)(int, int, int, LPWSAPROTOCOL_INFOA, GROUP, DWORD);
      const auto create = reinterpret_cast<CreateSocket>(GetProcAddress(GetModuleHandleW(L"ws2_32.dll"), "WSASocketA"));
      socket = create(address->ai_family, SOCK_DGRAM, IPPROTO_UDP, nullptr, 0, WSA_FLAG_OVERLAPPED);
    } else {
      socket = WSASocketW(address->ai_family, SOCK_DGRAM, IPPROTO_UDP, nullptr, 0,
                         WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT);
    }
    bool exchanged = false;
    if (socket != INVALID_SOCKET) {
      DWORD timeout = 2000;
      setsockopt(socket, SOL_SOCKET, SO_RCVTIMEO,
                 reinterpret_cast<const char*>(&timeout), sizeof(timeout));
      char response[5]{};
      exchanged = sendto(socket, "latch", 5, 0, address->ai_addr,
                         static_cast<int>(address->ai_addrlen)) == 5 &&
          recvfrom(socket, response, 5, 0, nullptr, nullptr) == 5 &&
          memcmp(response, "latch", 5) == 0;
      closesocket(socket);
    }
    FreeAddrInfoW(addresses);
    WSACleanup();
    return exchanged == (operation == L"network-udp-echo") ? 0 : 5;
  }
  if (operation == L"network-listen" || operation == L"network-listen-echo") {
    if (argc != 4 && argc != 5) return 2;
    const int hold_ms = _wtoi(argv[3]);
    if (hold_ms < 1 || hold_ms > 15000) return 2;
    const bool ipv6 = argc == 5 && std::wcscmp(argv[4], L"ipv6") == 0;
    if (argc == 5 && !ipv6) return 2;
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data)) return 3;
    SOCKET listener = ::socket(ipv6 ? AF_INET6 : AF_INET, SOCK_STREAM,
                               IPPROTO_TCP);
    int error = listener == INVALID_SOCKET ? WSAGetLastError() : 0;
    sockaddr_storage address{};
    int address_size = 0;
    if (ipv6) {
      DWORD only_v6 = 1;
      if (!error && setsockopt(listener, IPPROTO_IPV6, IPV6_V6ONLY,
                               reinterpret_cast<const char*>(&only_v6),
                               sizeof(only_v6)))
        error = WSAGetLastError();
      auto* target = reinterpret_cast<sockaddr_in6*>(&address);
      target->sin6_family = AF_INET6;
      target->sin6_addr = in6addr_loopback;
      address_size = sizeof(sockaddr_in6);
    } else {
      auto* target = reinterpret_cast<sockaddr_in*>(&address);
      target->sin_family = AF_INET;
      target->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      address_size = sizeof(sockaddr_in);
    }
    if (!error && bind(listener, reinterpret_cast<sockaddr*>(&address),
                       address_size))
      error = WSAGetLastError();
    if (!error && listen(listener, 1)) error = WSAGetLastError();
    int length = sizeof(address);
    if (!error && getsockname(listener, reinterpret_cast<sockaddr*>(&address),
                              &length))
      error = WSAGetLastError();
    const auto port = ipv6
                          ? ntohs(reinterpret_cast<sockaddr_in6*>(&address)
                                      ->sin6_port)
                          : ntohs(reinterpret_cast<sockaddr_in*>(&address)
                                      ->sin_port);
    const std::string status = error
                                   ? "BLOCKED " + std::to_string(error)
                                   : (ipv6 ? "LISTEN6 " : "LISTEN ") +
                                         std::to_string(port);
    HANDLE marker = CreateFileW(argv[2], GENERIC_WRITE, FILE_SHARE_READ,
                                nullptr, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
                                nullptr);
    if (marker == INVALID_HANDLE_VALUE) {
      if (listener != INVALID_SOCKET) closesocket(listener);
      WSACleanup();
      return 4;
    }
    DWORD written = 0;
    const BOOL stored = WriteFile(marker, status.data(),
                                 static_cast<DWORD>(status.size()), &written,
                                 nullptr);
    CloseHandle(marker);
    if (!stored || written != status.size()) {
      if (listener != INVALID_SOCKET) closesocket(listener);
      WSACleanup();
      return 5;
    }
    if (!error && operation == L"network-listen-echo") {
      fd_set readable;
      FD_ZERO(&readable); FD_SET(listener, &readable);
      timeval timeout{hold_ms / 1000, (hold_ms % 1000) * 1000};
      if (select(0, &readable, nullptr, nullptr, &timeout) <= 0) error = WSAETIMEDOUT;
      SOCKET client = !error ? accept(listener, nullptr, nullptr) : INVALID_SOCKET;
      if (!error && client == INVALID_SOCKET) error = WSAGetLastError();
      if (client != INVALID_SOCKET) {
        DWORD io_timeout = 3000;
        setsockopt(client, SOL_SOCKET, SO_RCVTIMEO,
                   reinterpret_cast<const char*>(&io_timeout), sizeof(io_timeout));
        char payload[5]{};
        int received = 0;
        while (received < sizeof(payload)) {
          const int count = recv(client, payload + received, sizeof(payload) - received, 0);
          if (count <= 0) { error = WSAECONNRESET; break; }
          received += count;
        }
        if (!error && (memcmp(payload, "latch", sizeof(payload)) ||
            send(client, payload, sizeof(payload), 0) != sizeof(payload))) error = WSAECONNRESET;
        closesocket(client);
      }
    } else if (!error) Sleep(static_cast<DWORD>(hold_ms));
    if (listener != INVALID_SOCKET) closesocket(listener);
    WSACleanup();
    std::printf("%s\n", status.c_str());
    return error == WSAEACCES || !error ? 0 : 6;
  }
  if (operation == L"spawn-x86") {
    if (argc != 3) return 2;
    std::wstring command = L"\"" + std::wstring(argv[2]) +
                           L"\" /d /c exit 0";
    STARTUPINFOW startup{};
    startup.cb = sizeof(startup);
    PROCESS_INFORMATION child{};
    if (!CreateProcessW(argv[2], command.data(), nullptr, nullptr, FALSE,
                        CREATE_NO_WINDOW, nullptr, nullptr, &startup, &child)) {
      std::printf("X86_DESCENDANT_REFUSED error=%lu\n", GetLastError());
      return 10;
    }
    BOOL in_job = FALSE;
    HANDLE token = nullptr;
    DWORD app_container = 0;
    DWORD returned = 0;
    const bool isolated =
        IsProcessInJob(child.hProcess, nullptr, &in_job) && in_job &&
        OpenProcessToken(child.hProcess, TOKEN_QUERY, &token) &&
        GetTokenInformation(token, TokenIsAppContainer, &app_container,
                            sizeof(app_container), &returned) &&
        app_container != 0;
    if (token) CloseHandle(token);
    if (!isolated) {
      TerminateProcess(child.hProcess, 125);
      WaitForSingleObject(child.hProcess, 5000);
      CloseHandle(child.hThread);
      CloseHandle(child.hProcess);
      std::puts("FAIL x86 descendant escaped its AppContainer job");
      return 1;
    }
    const DWORD wait = WaitForSingleObject(child.hProcess, 5000);
    DWORD exit_code = 0;
    const bool exited = wait == WAIT_OBJECT_0 &&
                        GetExitCodeProcess(child.hProcess, &exit_code) &&
                        exit_code == 0;
    if (!exited) TerminateProcess(child.hProcess, 125);
    if (!exited) WaitForSingleObject(child.hProcess, 5000);
    CloseHandle(child.hThread);
    CloseHandle(child.hProcess);
    if (!exited) {
      std::puts("FAIL x86 descendant did not exit cleanly");
      return 1;
    }
    std::puts("X86_DESCENDANT_STARTED appcontainer=1 job=1 exit=0");
    return 0;
  }
  if (operation == L"bulk-mutate") {
    if (argc != 4) return 2;
    const int count = _wtoi(argv[3]);
    if (count < 1 || count > 10000) return 2;
    for (int i = 0; i < count; ++i) {
      const std::wstring path = std::wstring(argv[2]) + L"\\sandbox-" +
                                std::to_wstring(i) + L".txt";
      HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr,
                                CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
      if (file == INVALID_HANDLE_VALUE) return 3;
      constexpr char content[] = "sandbox mutation";
      DWORD written = 0;
      const BOOL stored = WriteFile(file, content, sizeof(content) - 1,
                                    &written, nullptr);
      CloseHandle(file);
      if (!stored || written != sizeof(content) - 1) return 4;
      if (i % 2 == 0) {
        const std::wstring renamed = path + L".renamed";
        if (!MoveFileExW(path.c_str(), renamed.c_str(), MOVEFILE_WRITE_THROUGH))
          return 5;
      }
      if (i % 4 == 1 && !DeleteFileW(path.c_str())) return 6;
    }
    std::printf("Created and mutated %d workspace entries\n", count);
    return 0;
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
