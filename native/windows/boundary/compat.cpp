// Compatibility only: removing these hooks does not remove the token or job.
// They expose only inherited NUL and KsecDD handles.
#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <aclapi.h>
#include <sddl.h>
#include <cwchar>
#include <cstdio>
#include <cstring>
#include <initializer_list>
#include <string>
#include <utility>
#include "detours.h"
#include "handles.h"
#include "broker_protocol.h"

namespace {
decltype(&NtCreateFile) real_create = nullptr;
decltype(&NtOpenFile) real_open = nullptr;
using QueryAttributes = NTSTATUS (NTAPI*)(POBJECT_ATTRIBUTES, FILE_BASIC_INFO*);
QueryAttributes real_query_attributes = nullptr;
struct NetworkOpenInformation {
  LARGE_INTEGER CreationTime, LastAccessTime, LastWriteTime, ChangeTime;
  LARGE_INTEGER AllocationSize, EndOfFile;
  ULONG FileAttributes;
};
using QueryFullAttributes = NTSTATUS (NTAPI*)(POBJECT_ATTRIBUTES, NetworkOpenInformation*);
QueryFullAttributes real_query_full_attributes = nullptr;
auto real_process = CreateProcessW;
auto real_final_path = GetFinalPathNameByHandleW;
auto real_attributes = GetFileAttributesW;
auto real_attributes_ex = GetFileAttributesExW;
auto real_socket = ::socket;
auto real_wsa_socket = WSASocketW;
using WsaSocketA = SOCKET (WSAAPI*)(int, int, int, LPWSAPROTOCOL_INFOA, GROUP, DWORD);
WsaSocketA real_wsa_socket_a = nullptr;
std::wstring initial_cwd;
HANDLE null_handle = nullptr;
HANDLE ksec_handle = nullptr;
HANDLE read_broker = nullptr;
HANDLE broker_mutex = nullptr;
thread_local bool using_broker = false;
ULONGLONG broker_sequence = 0;
HANDLE ancestor_handles[latch_max_ancestors]{};
DWORD ancestor_count = 0;
ULONGLONG workspace_volume = 0;
wchar_t workspace_drive = 0;
char hook_path[MAX_PATH]{};

// All callers hold the inherited transaction mutex across the exchange.
bool broker_exchange(LatchReadRequest& request, LatchReadResponse& response) {
  request.version = latch_broker_version;
  request.process_id = GetCurrentProcessId();
  request.request_id = ++broker_sequence;
  DWORD bytes = 0;
  const DWORD size = latch_read_request_size(request.path_length);
  if (!WriteFile(read_broker, &request, size, &bytes, nullptr) || bytes != size)
    return false;
  for (;;) {
    if (!ReadFile(read_broker, &response, sizeof(response), &bytes, nullptr) ||
        bytes != sizeof(response) || response.version != latch_broker_version)
      return false;
    if (response.process_id == request.process_id &&
        response.request_id == request.request_id) return true;
  }
}

SOCKET broker_socket(int family, int type, int protocol, DWORD flags) {
  if (!read_broker || !broker_mutex || using_broker) return INVALID_SOCKET;
  using_broker = true;
  struct Reset { ~Reset() { using_broker = false; } } reset;
  const DWORD waited = WaitForSingleObject(broker_mutex, 10000);
  if (waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED) return INVALID_SOCKET;
  struct Unlock { ~Unlock() { ReleaseMutex(broker_mutex); } } unlock;
  LatchReadRequest request{};
  request.operation = LatchBrokerOperation::socket_create;
  request.family = family;
  request.socket_type = type;
  request.protocol = protocol;
  request.socket_flags = flags;
  LatchReadResponse response{};
  if (!broker_exchange(request, response) || response.error) return INVALID_SOCKET;
  const SOCKET socket = real_wsa_socket(FROM_PROTOCOL_INFO, FROM_PROTOCOL_INFO,
      FROM_PROTOCOL_INFO, &response.socket_information, 0, flags);
  const int error = socket == INVALID_SOCKET ? WSAGetLastError() : 0;
  request.operation = LatchBrokerOperation::socket_release;
  request.socket_ticket = response.socket_ticket;
  if (!broker_exchange(request, response) || response.error) {
    if (socket != INVALID_SOCKET) closesocket(socket);
    WSASetLastError(WSAEACCES);
    return INVALID_SOCKET;
  }
  if (error) WSASetLastError(error);
  return socket;
}

SOCKET WSAAPI socket_open(int family, int type, int protocol) {
  const SOCKET value = broker_socket(family, type, protocol, WSA_FLAG_OVERLAPPED);
  return value != INVALID_SOCKET ? value : real_socket(family, type, protocol);
}
SOCKET WSAAPI wsa_socket_open(int family, int type, int protocol,
                            LPWSAPROTOCOL_INFOW info, GROUP group, DWORD flags) {
  const SOCKET value = !info && !group ? broker_socket(family, type, protocol, flags) : INVALID_SOCKET;
  return value != INVALID_SOCKET ? value : real_wsa_socket(family, type, protocol, info, group, flags);
}
SOCKET WSAAPI wsa_socket_open_a(int family, int type, int protocol,
                              LPWSAPROTOCOL_INFOA info, GROUP group, DWORD flags) {
  const SOCKET value = !info && !group ? broker_socket(family, type, protocol, flags) : INVALID_SOCKET;
  return value != INVALID_SOCKET ? value : real_wsa_socket_a(family, type, protocol, info, group, flags);
}

bool named(POBJECT_ATTRIBUTES attributes, const wchar_t* expected) {
  if (!attributes || attributes->RootDirectory || !attributes->ObjectName) return false;
  const auto* name = attributes->ObjectName;
  const size_t length = std::wcslen(expected);
  return name->Length == length * sizeof(wchar_t) &&
      _wcsnicmp(name->Buffer, expected, length) == 0;
}

bool duplicate(HANDLE source, PHANDLE target, PIO_STATUS_BLOCK status,
               bool inherit = false) {
  if (!source || !DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                                  target, 0, inherit, DUPLICATE_SAME_ACCESS)) return false;
  status->Status = 0;
  status->Information = FILE_OPENED;
  return true;
}

bool broker_read(POBJECT_ATTRIBUTES attributes, ACCESS_MASK access, ULONG share,
                 ULONG options, PHANDLE handle, PIO_STATUS_BLOCK status) {
  if (!read_broker || !broker_mutex || using_broker || !attributes ||
      !attributes->ObjectName || !handle || !status ||
      attributes->ObjectName->Length % sizeof(wchar_t)) return false;
  using_broker = true;
  struct Reset { ~Reset() { using_broker = false; } } reset;
  const auto* name = attributes->ObjectName;
  std::wstring path(name->Buffer, name->Length / sizeof(wchar_t));
  if (attributes->RootDirectory) {
    wchar_t root[32768]{};
    const DWORD length = GetFinalPathNameByHandleW(attributes->RootDirectory, root,
                                                 32768, FILE_NAME_NORMALIZED);
    if (!length || length >= 32768 || path.starts_with(L"\\")) return false;
    path = std::wstring(root, length) + L"\\" + path;
  }
  if (path.starts_with(L"\\??\\") || path.starts_with(L"\\\\?\\"))
    path.erase(0, 4);
  if (path.size() < 3 || path.size() >= 32768 || path[1] != L':') return false;
  const DWORD waited = WaitForSingleObject(broker_mutex, 10000);
  if (waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED) return false;
  struct Unlock { ~Unlock() { ReleaseMutex(broker_mutex); } } unlock;
  LatchReadRequest request{};
  request.access = access;
  request.share = share;
  request.options = options;
  request.path_length = static_cast<DWORD>(path.size());
  std::memcpy(request.path, path.data(), path.size() * sizeof(wchar_t));
  // A process killed while holding the mutex may leave its response queued.
  // Only the matching process/sequence may consume a returned capability.
  LatchReadResponse response{};
  if (!broker_exchange(request, response) || response.error || !response.file) return false;
  *handle = response.file;
  status->Status = 0;
  status->Information = FILE_OPENED;
  return true;
}

HANDLE broker_attributes(LPCWSTR path) {
  if (!path) return nullptr;
  std::wstring name(path);
  if (!name.starts_with(L"\\\\?\\")) {
    wchar_t full[32768]{};
    const DWORD length = GetFullPathNameW(path, 32768, full, nullptr);
    if (!length || length >= 32768) return nullptr;
    name.assign(full, length);
  }
  UNICODE_STRING text{};
  text.Buffer = name.data();
  if (name.size() >= 32768) return nullptr;
  text.Length = static_cast<USHORT>(name.size() * sizeof(wchar_t));
  text.MaximumLength = text.Length;
  OBJECT_ATTRIBUTES attributes{sizeof(attributes), nullptr, &text,
                               OBJ_CASE_INSENSITIVE, nullptr, nullptr};
  HANDLE file = nullptr;
  IO_STATUS_BLOCK io{};
  if (!broker_read(&attributes, FILE_READ_ATTRIBUTES | SYNCHRONIZE,
       FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_SYNCHRONOUS_IO_NONALERT,
       &file, &io)) return nullptr;
  return file;
}

NTSTATUS NTAPI query_attributes(POBJECT_ATTRIBUTES attributes, FILE_BASIC_INFO* output) {
  const NTSTATUS result = real_query_attributes(attributes, output);
  if (result != static_cast<NTSTATUS>(0xc0000022u) || !output) return result;
  HANDLE file = nullptr;
  IO_STATUS_BLOCK io{};
  if (!broker_read(attributes, FILE_READ_ATTRIBUTES | SYNCHRONIZE,
      FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_SYNCHRONOUS_IO_NONALERT, &file, &io))
    return result;
  FILE_BASIC_INFO info{};
  const BOOL queried = GetFileInformationByHandleEx(file, FileBasicInfo, &info, sizeof(info));
  CloseHandle(file);
  if (!queried) return result;
  output->CreationTime = info.CreationTime;
  output->LastAccessTime = info.LastAccessTime;
  output->LastWriteTime = info.LastWriteTime;
  output->ChangeTime = info.ChangeTime;
  output->FileAttributes = info.FileAttributes;
  return 0;
}

NTSTATUS NTAPI query_full_attributes(POBJECT_ATTRIBUTES attributes, NetworkOpenInformation* output) {
  const NTSTATUS result = real_query_full_attributes(attributes, output);
  if (result != static_cast<NTSTATUS>(0xc0000022u) || !output) return result;
  HANDLE file = nullptr;
  IO_STATUS_BLOCK io{};
  if (!broker_read(attributes, FILE_READ_ATTRIBUTES | SYNCHRONIZE,
      FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_SYNCHRONOUS_IO_NONALERT, &file, &io))
    return result;
  FILE_BASIC_INFO basic{};
  FILE_STANDARD_INFO standard{};
  const BOOL queried = GetFileInformationByHandleEx(file, FileBasicInfo, &basic, sizeof(basic)) &&
      GetFileInformationByHandleEx(file, FileStandardInfo, &standard, sizeof(standard));
  CloseHandle(file);
  if (!queried) return result;
  *output = {basic.CreationTime, basic.LastAccessTime, basic.LastWriteTime, basic.ChangeTime,
             standard.AllocationSize, standard.EndOfFile, basic.FileAttributes};
  return 0;
}

DWORD WINAPI final_path(HANDLE handle, LPWSTR output, DWORD capacity, DWORD flags) {
  const DWORD length = real_final_path(handle, output, capacity, flags);
  if (length || GetLastError() != ERROR_ACCESS_DENIED ||
      (flags & 0x7) != VOLUME_NAME_DOS ||
      !workspace_drive) return length;
  FILE_ID_INFO file_id{};
  if (!GetFileInformationByHandleEx(handle, FileIdInfo, &file_id,
                                    sizeof(file_id)) ||
      file_id.VolumeSerialNumber != workspace_volume) return 0;
  wchar_t relative[32768]{};
  const DWORD relative_length = real_final_path(handle, relative, 32768,
      FILE_NAME_OPENED | VOLUME_NAME_NONE);
  if (!relative_length || relative_length >= 32768) return 0;
  std::wstring absolute = {92, 92, 63, 92};
  absolute += workspace_drive;
  absolute += L":";
  absolute.append(relative, relative_length);
  if (capacity <= absolute.size()) {
    SetLastError(ERROR_INSUFFICIENT_BUFFER);
    return static_cast<DWORD>(absolute.size() + 1);
  }
  std::memcpy(output, absolute.c_str(), (absolute.size() + 1) * sizeof(wchar_t));
  SetLastError(ERROR_SUCCESS);
  return static_cast<DWORD>(absolute.size());
}

// Some AppContainer tokens cannot stat even the volume root. Answer only
// metadata queries for known directory ancestors of the OS-selected cwd.
// Actual opens, reads and writes still go through the restricted token.
bool cwd_ancestor(LPCWSTR path) {
  if (!path || initial_cwd.empty()) return false;
  std::wstring name(path);
  if (name.starts_with(L"\\\\?\\")) name.erase(0, 4);
  for (auto& ch : name) if (ch == L'/') ch = L'\\';
  while (name.size() > 3 && name.back() == L'\\') name.pop_back();
  if (name.size() < 3 || name[1] != L':' || name[2] != L'\\' ||
      towupper(name[0]) != towupper(workspace_drive) ||
      name.size() >= initial_cwd.size() ||
      _wcsnicmp(name.c_str(), initial_cwd.c_str(), name.size()) != 0) return false;
  return name.size() == 3 || initial_cwd[name.size()] == L'\\';
}

bool metadata_ancestor(POBJECT_ATTRIBUTES attributes, ACCESS_MASK access,
                       PHANDLE handle, PIO_STATUS_BLOCK status) {
  if (!attributes || attributes->RootDirectory || !attributes->ObjectName ||
      (access & ~(FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY | SYNCHRONIZE)) != 0)
    return false;
  const auto* object = attributes->ObjectName;
  std::wstring native(object->Buffer, object->Length / sizeof(wchar_t));
  if (!native.starts_with(L"\\??\\")) return false;
  std::wstring requested = native.substr(4);
  for (auto& ch : requested) if (ch == L'/') ch = L'\\';
  while (requested.size() > 3 && requested.back() == L'\\') requested.pop_back();
  if (!cwd_ancestor(requested.c_str())) return false;
  std::wstring parent = initial_cwd;
  for (DWORD i = 0; i < ancestor_count; ++i) {
    const size_t separator = parent.find_last_of(L"\\/");
    if (separator == std::wstring::npos || separator < 2) break;
    parent.resize(separator == 2 ? 3 : separator);
    if (_wcsicmp(parent.c_str(), requested.c_str()) == 0 &&
        duplicate(ancestor_handles[i], handle, status)) return true;
  }
  return false;
}

DWORD WINAPI attributes(LPCWSTR path) {
  const DWORD value = real_attributes(path);
  if (value != INVALID_FILE_ATTRIBUTES || GetLastError() != ERROR_ACCESS_DENIED)
    return value;
  if (cwd_ancestor(path)) {
    SetLastError(ERROR_SUCCESS);
    return FILE_ATTRIBUTE_DIRECTORY;
  }
  HANDLE file = broker_attributes(path);
  FILE_BASIC_INFO info{};
  if (file) {
    const BOOL queried = GetFileInformationByHandleEx(file, FileBasicInfo, &info, sizeof(info));
    CloseHandle(file);
    if (queried) {
      SetLastError(ERROR_SUCCESS);
      return info.FileAttributes;
    }
  }
  SetLastError(ERROR_ACCESS_DENIED);
  return value;
}

BOOL WINAPI attributes_ex(LPCWSTR path, GET_FILEEX_INFO_LEVELS level, LPVOID output) {
  const BOOL value = real_attributes_ex(path, level, output);
  if (value || GetLastError() != ERROR_ACCESS_DENIED ||
      level != GetFileExInfoStandard || !output) return value;
  auto* data = static_cast<WIN32_FILE_ATTRIBUTE_DATA*>(output);
  if (!cwd_ancestor(path)) {
    HANDLE file = broker_attributes(path);
    BY_HANDLE_FILE_INFORMATION info{};
    if (file) {
      const BOOL queried = GetFileInformationByHandle(file, &info);
      CloseHandle(file);
      if (queried) {
        *data = {info.dwFileAttributes, info.ftCreationTime, info.ftLastAccessTime,
                 info.ftLastWriteTime, info.nFileSizeHigh, info.nFileSizeLow};
        SetLastError(ERROR_SUCCESS);
        return TRUE;
      }
    }
    SetLastError(ERROR_ACCESS_DENIED);
    return FALSE;
  }
  *data = {};
  data->dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY;
  SetLastError(ERROR_SUCCESS);
  return TRUE;
}

NTSTATUS NTAPI create(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
    PIO_STATUS_BLOCK io, PLARGE_INTEGER size, ULONG flags, ULONG share,
    ULONG disposition, ULONG options, PVOID ea, ULONG length) {
  if (named(attributes, L"\\??\\NUL") &&
      duplicate(null_handle, handle, io,
                (attributes->Attributes & OBJ_INHERIT) != 0)) return 0;
  const NTSTATUS result = real_create(handle, access, attributes, io, size,
                                      flags, share, disposition, options, ea, length);
  if (result == static_cast<NTSTATUS>(0xc0000022u) &&
      metadata_ancestor(attributes, access, handle, io)) return 0;
  if (result == static_cast<NTSTATUS>(0xc0000022u) && disposition == FILE_OPEN &&
      !length && broker_read(attributes, access, share, options, handle, io)) return 0;
  return result;
}

NTSTATUS NTAPI open(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
    PIO_STATUS_BLOCK io, ULONG share, ULONG options) {
  if (named(attributes, L"\\Device\\KsecDD") &&
      duplicate(ksec_handle, handle, io,
                (attributes->Attributes & OBJ_INHERIT) != 0)) return 0;
  const NTSTATUS result = real_open(handle, access, attributes, io, share, options);
  if (result == static_cast<NTSTATUS>(0xc0000022u) &&
      metadata_ancestor(attributes, access, handle, io)) return 0;
  if (result == static_cast<NTSTATUS>(0xc0000022u) &&
      broker_read(attributes, access, share, options, handle, io)) return 0;
  return result;
}

BOOL WINAPI spawn(LPCWSTR app, LPWSTR command, LPSECURITY_ATTRIBUTES process_attributes,
    LPSECURITY_ATTRIBUTES thread_attributes, BOOL inherit, DWORD flags,
    LPVOID environment, LPCWSTR cwd, LPSTARTUPINFOW startup, LPPROCESS_INFORMATION process) {
  // This compatibility DLL is built for the current (x64) process. Detours
  // cannot inject it into a WOW64 child; reject that child before attempting
  // cross-bitness injection, which can otherwise leave CreateProcess waiting.
  DWORD binary_type = 0;
  if (app && GetBinaryTypeW(app, &binary_type) &&
      binary_type == SCS_32BIT_BINARY) {
    SetLastError(ERROR_NOT_SUPPORTED);
    return FALSE;
  }
  STARTUPINFOW inherited_startup{};
  GetStartupInfoW(&inherited_startup);
  LPWSTR original_desktop = startup->lpDesktop;
  struct RestoreDesktop {
    LPSTARTUPINFOW startup;
    LPWSTR original;
    ~RestoreDesktop() { startup->lpDesktop = original; }
  } restore_desktop{startup, original_desktop};

  if (!original_desktop) startup->lpDesktop = inherited_startup.lpDesktop;
  STARTUPINFOEXW extended{};
  alignas(void*) BYTE attribute_storage[1024]{};
  LPPROC_THREAD_ATTRIBUTE_LIST list = nullptr;
  HANDLE standard_handles[3]{};
  struct HandleCleanup {
    HANDLE* handles;
    LPPROC_THREAD_ATTRIBUTE_LIST* attributes;
    ~HandleCleanup() {
      if (*attributes) DeleteProcThreadAttributeList(*attributes);
      for (size_t i = 0; i < 3; ++i) if (handles[i]) CloseHandle(handles[i]);
    }
  } cleanup{standard_handles, &list};
  if (!(flags & EXTENDED_STARTUPINFO_PRESENT) && (startup->dwFlags & STARTF_USESTDHANDLES)) {
    extended.StartupInfo = *startup;
    extended.StartupInfo.lpReserved = nullptr;
    extended.StartupInfo.cbReserved2 = 0;
    extended.StartupInfo.lpReserved2 = nullptr;
    extended.StartupInfo.cb = sizeof(extended);
    SIZE_T size = sizeof(attribute_storage);
    list = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(attribute_storage);
    if (!InitializeProcThreadAttributeList(list, 1, 0, &size)) { list = nullptr; return FALSE; }
    SIZE_T count = 0;
    HANDLE* outputs[] = {&extended.StartupInfo.hStdInput, &extended.StartupInfo.hStdOutput,
                          &extended.StartupInfo.hStdError};
    for (HANDLE* handle : outputs) {
      if (*handle && *handle != INVALID_HANDLE_VALUE) {
        HANDLE copy = nullptr;
        if (!DuplicateHandle(GetCurrentProcess(), *handle, GetCurrentProcess(), &copy,
                              0, TRUE, DUPLICATE_SAME_ACCESS)) return FALSE;
        standard_handles[count++] = copy;
        *handle = copy;
      }
    }
    if (!UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                  standard_handles, count * sizeof(HANDLE), nullptr, nullptr)) {
      return FALSE;
    }
    extended.lpAttributeList = list;
  }
  const BOOL created = DetourCreateProcessWithDllExW(app, command, process_attributes, thread_attributes,
      inherit, flags | CREATE_SUSPENDED | (list ? EXTENDED_STARTUPINFO_PRESENT : 0),
      environment, cwd, list ? &extended.StartupInfo : startup, process, hook_path,
      real_process);
  if (list) {
    DeleteProcThreadAttributeList(list);
    list = nullptr;
    for (HANDLE& handle : standard_handles) if (handle) { CloseHandle(handle); handle = nullptr; }
  }
  startup->lpDesktop = original_desktop;
  if (!created) return FALSE;
  LatchHandles handles{};
  handles.workspace_volume = workspace_volume;
  handles.workspace_drive = workspace_drive;
  handles.ancestor_count = ancestor_count;
  for (const auto& entry : {std::pair{read_broker, &handles.read_broker},
                            std::pair{broker_mutex, &handles.broker_mutex}}) {
    if (entry.first && !DuplicateHandle(GetCurrentProcess(), entry.first,
          process->hProcess, entry.second, 0, FALSE, DUPLICATE_SAME_ACCESS)) {
      TerminateProcess(process->hProcess, 125);
      WaitForSingleObject(process->hProcess, 5000);
      CloseHandle(process->hThread);
      CloseHandle(process->hProcess);
      *process = {};
      return FALSE;
    }
  }
  for (DWORD i = 0; i < ancestor_count; ++i)
    if (!DuplicateHandle(GetCurrentProcess(), ancestor_handles[i], process->hProcess,
                         &handles.ancestors[i], 0, FALSE, DUPLICATE_SAME_ACCESS)) {
      TerminateProcess(process->hProcess, 125);
      WaitForSingleObject(process->hProcess, 5000);
      CloseHandle(process->hThread);
      CloseHandle(process->hProcess);
      *process = {};
      return FALSE;
    }
  if (!DuplicateHandle(GetCurrentProcess(), null_handle, process->hProcess,
                        &handles.null_device, 0, FALSE, DUPLICATE_SAME_ACCESS) ||
      !DuplicateHandle(GetCurrentProcess(), ksec_handle, process->hProcess,
                        &handles.crypto_device, 0, FALSE, DUPLICATE_SAME_ACCESS) ||
      !DetourCopyPayloadToProcess(process->hProcess, latch_handles_id, &handles,
                                  sizeof(handles)) ||
      (!(flags & CREATE_SUSPENDED) && ResumeThread(process->hThread) == static_cast<DWORD>(-1))) {
    const DWORD error = GetLastError();
    TerminateProcess(process->hProcess, 125);
    WaitForSingleObject(process->hProcess, 5000);
    CloseHandle(process->hThread);
    CloseHandle(process->hProcess);
    *process = {};
    SetLastError(error);
    return FALSE;
  }
  return TRUE;
}

HANDLE inherited(const wchar_t* name) {
  wchar_t raw[40]{};
  const DWORD length = GetEnvironmentVariableW(name, raw, 40);
  if (!length || length >= 40) return nullptr;
  return reinterpret_cast<HANDLE>(_wcstoui64(raw, nullptr, 16));
}

BOOL init_failed(const char* stage) {
  // DllMain has no caller to report a hook failure to. The fixture captures
  // stderr from the child, including when the loader returns DLL_INIT_FAILED.
  const HANDLE output = GetStdHandle(STD_ERROR_HANDLE);
  if (output && output != INVALID_HANDLE_VALUE) {
    DWORD written = 0;
    const char prefix[] = "Latch compatibility initialization failed: ";
    WriteFile(output, prefix, sizeof(prefix) - 1, &written, nullptr);
    WriteFile(output, stage, static_cast<DWORD>(std::strlen(stage)), &written, nullptr);
    const char suffix[] = "\r\n";
    WriteFile(output, suffix, sizeof(suffix) - 1, &written, nullptr);
  }
  return FALSE;
}
} // namespace

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID) {
  if (DetourIsHelperProcess() || reason != DLL_PROCESS_ATTACH) return TRUE;

  const DWORD length = GetModuleFileNameA(module, hook_path, MAX_PATH);
  if (!length || length >= MAX_PATH) return init_failed("module path");
  null_handle = inherited(L"LATCH_NULL_HANDLE");
  ksec_handle = inherited(L"LATCH_KSEC_HANDLE");
  DWORD payload_size = 0;
  const auto* handles = static_cast<const LatchHandles*>(
      DetourFindPayloadEx(latch_handles_id, &payload_size));
  if (handles && payload_size == sizeof(LatchHandles)) {
    null_handle = handles->null_device;
    ksec_handle = handles->crypto_device;
    read_broker = handles->read_broker;
    broker_mutex = handles->broker_mutex;
    workspace_volume = handles->workspace_volume;
    workspace_drive = handles->workspace_drive;
    if (handles->ancestor_count > latch_max_ancestors) return init_failed("ancestor count");
    ancestor_count = handles->ancestor_count;
    for (DWORD i = 0; i < ancestor_count; ++i)
      ancestor_handles[i] = handles->ancestors[i];
  }
  wchar_t cwd[32768]{};
  const DWORD cwd_length = GetCurrentDirectoryW(32768, cwd);
  if (cwd_length && cwd_length < 32768) initial_cwd.assign(cwd, cwd_length);
  const HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
  real_wsa_socket_a = reinterpret_cast<WsaSocketA>(
      GetProcAddress(GetModuleHandleW(L"ws2_32.dll"), "WSASocketA"));
  real_open = reinterpret_cast<decltype(real_open)>(GetProcAddress(ntdll, "NtOpenFile"));
  real_create = reinterpret_cast<decltype(real_create)>(GetProcAddress(ntdll, "NtCreateFile"));
  real_query_attributes = reinterpret_cast<QueryAttributes>(GetProcAddress(ntdll, "NtQueryAttributesFile"));
  real_query_full_attributes = reinterpret_cast<QueryFullAttributes>(GetProcAddress(ntdll, "NtQueryFullAttributesFile"));

  if (!real_open || !real_create || !real_query_attributes || !real_query_full_attributes ||
      !real_wsa_socket_a)
    return init_failed("native exports");
  if (!DetourRestoreAfterWith()) return init_failed("restore process image");
  if (DetourTransactionBegin()) return init_failed("begin hooks");
  if (DetourUpdateThread(GetCurrentThread())) return init_failed("update thread");
  if (DetourAttach(reinterpret_cast<void**>(&real_socket), socket_open) ||
      DetourAttach(reinterpret_cast<void**>(&real_wsa_socket), wsa_socket_open) ||
      DetourAttach(reinterpret_cast<void**>(&real_wsa_socket_a), wsa_socket_open_a))
    return init_failed("network socket hooks");
  if (DetourAttach(reinterpret_cast<void**>(&real_create), create)) return init_failed("NtCreateFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_open), open)) return init_failed("NtOpenFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_query_attributes), query_attributes)) return init_failed("NtQueryAttributesFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_query_full_attributes), query_full_attributes)) return init_failed("NtQueryFullAttributesFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_final_path), final_path)) return init_failed("final path hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_attributes), attributes)) return init_failed("file attributes hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_attributes_ex), attributes_ex)) return init_failed("file attributes ex hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_process), spawn)) return init_failed("process hook");
  if (DetourTransactionCommit()) return init_failed("commit hooks");
  return TRUE;
}
