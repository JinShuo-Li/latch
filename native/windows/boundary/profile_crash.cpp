#include "profile_crash.h"
#include "detours.h"
#include "recovery.h"

namespace latch {
namespace {
const Recovery* active = nullptr;
auto real_create_file = &NtCreateFile;
using SetValue = NTSTATUS(NTAPI*)(HANDLE, PUNICODE_STRING, ULONG, ULONG,
                                  PVOID, ULONG);
SetValue real_set_value = nullptr;
using CreateKey = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
                                    ULONG, PUNICODE_STRING, ULONG, PULONG);
CreateKey real_create_key = nullptr;

NTSTATUS NTAPI create_key(PHANDLE key, ACCESS_MASK access,
    POBJECT_ATTRIBUTES attributes, ULONG title, PUNICODE_STRING kind,
    ULONG options, PULONG disposition) {
  const auto result = real_create_key(key, access, attributes, title, kind,
                                     options, disposition);
  if (result >= 0 && active && attributes && attributes->ObjectName) {
    const auto* name = attributes->ObjectName;
    const std::wstring_view path(name->Buffer, name->Length / sizeof(wchar_t));
    const auto& sid = active->package_sid();
    if (path.size() >= sid.size() &&
        _wcsnicmp(path.data() + path.size() - sid.size(), sid.c_str(), sid.size()) == 0)
      active->pause(L"profile-api-key");
  }
  return result;
}

NTSTATUS NTAPI create_file(PHANDLE handle, ACCESS_MASK access,
    POBJECT_ATTRIBUTES attributes, PIO_STATUS_BLOCK io, PLARGE_INTEGER size,
    ULONG flags, ULONG share, ULONG disposition, ULONG options, PVOID ea,
    ULONG length) {
  const auto result = real_create_file(handle, access, attributes, io, size,
      flags, share, disposition, options, ea, length);
  if (result >= 0 && active && io && io->Information == FILE_CREATED &&
      attributes && attributes->ObjectName &&
      (options & FILE_DIRECTORY_FILE)) {
    const auto* name = attributes->ObjectName;
    const std::wstring_view path(name->Buffer, name->Length / sizeof(wchar_t));
    const auto& profile = active->profile();
    if (path.size() >= profile.size() &&
        _wcsnicmp(path.data() + path.size() - profile.size(), profile.c_str(),
                  profile.size()) == 0) active->pause(L"profile-api-directory");
  }
  return result;
}

NTSTATUS NTAPI set_value(HANDLE key, PUNICODE_STRING name, ULONG title,
    ULONG type, PVOID data, ULONG bytes) {
  const auto result = real_set_value(key, name, title, type, data, bytes);
  if (result >= 0 && active && name && data && type == REG_SZ &&
      bytes >= sizeof(wchar_t)) {
    const std::wstring_view value_name(name->Buffer, name->Length / sizeof(wchar_t));
    const std::wstring_view value(static_cast<wchar_t*>(data),
        bytes / sizeof(wchar_t) - 1);
    if (value_name == L"Moniker" && value.size() == active->profile().size() &&
        _wcsnicmp(value.data(), active->profile().c_str(), value.size()) == 0)
      active->pause(L"profile-api-mapping");
  }
  return result;
}
}  // namespace

ProfileCrashHooks::ProfileCrashHooks(const Recovery& recovery) {
  active = &recovery;
  real_set_value = reinterpret_cast<SetValue>(
      GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetValueKey"));
  require(real_set_value != nullptr, L"fixture NtSetValueKey unavailable");
  real_create_key = reinterpret_cast<CreateKey>(
      GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateKey"));
  require(real_create_key != nullptr, L"fixture NtCreateKey unavailable");
  LONG code = DetourTransactionBegin();
  if (!code) code = DetourUpdateThread(GetCurrentThread());
  if (!code) code = DetourAttach(reinterpret_cast<PVOID*>(&real_create_file), create_file);
  if (!code) code = DetourAttach(reinterpret_cast<PVOID*>(&real_set_value), set_value);
  if (!code) code = DetourAttach(reinterpret_cast<PVOID*>(&real_create_key), create_key);
  if (code) {
    DetourTransactionAbort();
    fail(L"install profile crash hooks", static_cast<DWORD>(code));
  }
  code = DetourTransactionCommit();
  if (code) fail(L"commit profile crash hooks", static_cast<DWORD>(code));
}
ProfileCrashHooks::~ProfileCrashHooks() {
  active = nullptr;
  if (DetourTransactionBegin() != NO_ERROR) std::terminate();
  if (DetourUpdateThread(GetCurrentThread()) != NO_ERROR ||
      DetourDetach(reinterpret_cast<PVOID*>(&real_create_file), create_file) != NO_ERROR ||
      DetourDetach(reinterpret_cast<PVOID*>(&real_set_value), set_value) != NO_ERROR ||
      DetourDetach(reinterpret_cast<PVOID*>(&real_create_key), create_key) != NO_ERROR ||
      DetourTransactionCommit() != NO_ERROR) std::terminate();
}
}  // namespace latch
