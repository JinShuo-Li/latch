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
#include <initializer_list>
#include "detours.h"
#include "handles.h"

namespace {
decltype(&NtCreateFile) real_create = nullptr;
decltype(&NtOpenFile) real_open = nullptr;
auto real_process = CreateProcessW;
HANDLE null_handle = nullptr;
HANDLE ksec_handle = nullptr;
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

NTSTATUS NTAPI create(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
    PIO_STATUS_BLOCK io, PLARGE_INTEGER size, ULONG flags, ULONG share,
    ULONG disposition, ULONG options, PVOID ea, ULONG length) {
  if (named(attributes, L"\\??\\NUL") && duplicate(null_handle, handle, io)) return 0;
  return real_create(handle, access, attributes, io, size, flags, share,
                     disposition, options, ea, length);
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
} // namespace

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID) {
  if (DetourIsHelperProcess() || reason != DLL_PROCESS_ATTACH) return TRUE;

  const DWORD length = GetModuleFileNameA(module, hook_path, MAX_PATH);
  if (!length || length >= MAX_PATH) return FALSE;
  null_handle = inherited(L"LATCH_NULL_HANDLE");
  ksec_handle = inherited(L"LATCH_KSEC_HANDLE");
  DWORD payload_size = 0;
  const auto* handles = static_cast<const LatchHandles*>(
      DetourFindPayloadEx(latch_handles_id, &payload_size));
  if (handles && payload_size == sizeof(LatchHandles)) {
    null_handle = handles->null_device;
    ksec_handle = handles->crypto_device;
  }
  const HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
  real_open = reinterpret_cast<decltype(real_open)>(GetProcAddress(ntdll, "NtOpenFile"));
  real_create = reinterpret_cast<decltype(real_create)>(GetProcAddress(ntdll, "NtCreateFile"));

  if (!real_open || !real_create || !DetourRestoreAfterWith() ||
      DetourTransactionBegin() || DetourUpdateThread(GetCurrentThread()) ||
      DetourAttach(reinterpret_cast<void**>(&real_create), create) ||
      DetourAttach(reinterpret_cast<void**>(&real_open), open) ||
      DetourAttach(reinterpret_cast<void**>(&real_process), spawn) ||
      DetourTransactionCommit()) return FALSE;
  return TRUE;
}
