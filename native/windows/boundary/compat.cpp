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
#include "detours.h"
#include "handles.h"

namespace {
decltype(&NtCreateFile) real_create = nullptr;
decltype(&NtOpenFile) real_open = nullptr;
auto real_process = CreateProcessW;
auto real_final_path = GetFinalPathNameByHandleW;
auto real_attributes = GetFileAttributesW;
auto real_attributes_ex = GetFileAttributesExW;
std::wstring initial_cwd;
HANDLE null_handle = nullptr;
HANDLE ksec_handle = nullptr;
HANDLE ancestor_handles[latch_max_ancestors]{};
DWORD ancestor_count = 0;
ULONGLONG workspace_volume = 0;
wchar_t workspace_drive = 0;
char hook_path[MAX_PATH]{};

bool named(POBJECT_ATTRIBUTES attributes, const wchar_t* expected) {
  if (!attributes || attributes->RootDirectory || !attributes->ObjectName) return false;
  const auto* name = attributes->ObjectName;
  const size_t length = std::wcslen(expected);
  return name->Length == length * sizeof(wchar_t) &&
      _wcsnicmp(name->Buffer, expected, length) == 0;
}

bool duplicate(HANDLE source, PHANDLE target, PIO_STATUS_BLOCK status) {
  if (!source || !DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                                  target, 0, FALSE, DUPLICATE_SAME_ACCESS)) return false;
  status->Status = 0;
  status->Information = FILE_OPENED;
  return true;
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
      (access & ~(FILE_READ_ATTRIBUTES | FILE_READ_DATA | SYNCHRONIZE)) != 0)
    return false;
  const auto* object = attributes->ObjectName;
  std::wstring native(object->Buffer, object->Length / sizeof(wchar_t));
  if (!native.starts_with(L"\\??\\")) return false;
  const std::wstring requested = native.substr(4);
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
  if (value != INVALID_FILE_ATTRIBUTES || GetLastError() != ERROR_ACCESS_DENIED ||
      !cwd_ancestor(path)) return value;
  SetLastError(ERROR_SUCCESS);
  return FILE_ATTRIBUTE_DIRECTORY;
}

BOOL WINAPI attributes_ex(LPCWSTR path, GET_FILEEX_INFO_LEVELS level, LPVOID output) {
  const BOOL value = real_attributes_ex(path, level, output);
  if (value || GetLastError() != ERROR_ACCESS_DENIED ||
      level != GetFileExInfoStandard || !output || !cwd_ancestor(path)) return value;
  auto* data = static_cast<WIN32_FILE_ATTRIBUTE_DATA*>(output);
  *data = {};
  data->dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY;
  SetLastError(ERROR_SUCCESS);
  return TRUE;
}

NTSTATUS NTAPI create(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
    PIO_STATUS_BLOCK io, PLARGE_INTEGER size, ULONG flags, ULONG share,
    ULONG disposition, ULONG options, PVOID ea, ULONG length) {
  if (named(attributes, L"\\??\\NUL") && duplicate(null_handle, handle, io)) return 0;
  const NTSTATUS result = real_create(handle, access, attributes, io, size,
                                      flags, share, disposition, options, ea, length);
  if (result == static_cast<NTSTATUS>(0xc0000022u) &&
      metadata_ancestor(attributes, access, handle, io)) return 0;
  return result;
}

NTSTATUS NTAPI open(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
    PIO_STATUS_BLOCK io, ULONG share, ULONG options) {
  if (named(attributes, L"\\Device\\KsecDD") && duplicate(ksec_handle, handle, io)) return 0;
  return real_open(handle, access, attributes, io, share, options);
}

BOOL WINAPI spawn(LPCWSTR app, LPWSTR command, LPSECURITY_ATTRIBUTES process_attributes,
    LPSECURITY_ATTRIBUTES thread_attributes, BOOL inherit, DWORD flags,
    LPVOID environment, LPCWSTR cwd, LPSTARTUPINFOW startup, LPPROCESS_INFORMATION process) {
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
#ifdef LATCH_RECOVERY_TESTING
  const char entered[] = "Latch compatibility entered\r\n";
  DWORD entered_bytes = 0;
  WriteFile(GetStdHandle(STD_ERROR_HANDLE), entered, sizeof(entered) - 1,
            &entered_bytes, nullptr);
#endif

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
  real_open = reinterpret_cast<decltype(real_open)>(GetProcAddress(ntdll, "NtOpenFile"));
  real_create = reinterpret_cast<decltype(real_create)>(GetProcAddress(ntdll, "NtCreateFile"));

  if (!real_open || !real_create) return init_failed("native exports");
  if (!DetourRestoreAfterWith()) return init_failed("restore process image");
  if (DetourTransactionBegin()) return init_failed("begin hooks");
  if (DetourUpdateThread(GetCurrentThread())) return init_failed("update thread");
  if (DetourAttach(reinterpret_cast<void**>(&real_create), create)) return init_failed("NtCreateFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_open), open)) return init_failed("NtOpenFile hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_final_path), final_path)) return init_failed("final path hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_attributes), attributes)) return init_failed("file attributes hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_attributes_ex), attributes_ex)) return init_failed("file attributes ex hook");
  if (DetourAttach(reinterpret_cast<void**>(&real_process), spawn)) return init_failed("process hook");
  if (DetourTransactionCommit()) return init_failed("commit hooks");
  return TRUE;
}
