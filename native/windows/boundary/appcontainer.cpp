#include "appcontainer.h"

#include <objbase.h>
#include <userenv.h>

#include "recovery.h"
namespace latch {
AppContainer::AppContainer(Recovery& recovery) {
  recovery.prepare_profile();
  const auto& name = recovery.profile();
  const HRESULT code =
      CreateAppContainerProfile(name.c_str(), name.c_str(),
                                L"Disposable Latch boundary", nullptr, 0, &sid);
  if (FAILED(code))
    fail(L"create AppContainer profile", static_cast<DWORD>(code));
  recovery.pause(L"profile-unsealed");
}
AppContainer::~AppContainer() {
  if (sid) FreeSid(sid);
}
void validate_profile_mapping(const std::wstring& profile,
                              const std::wstring& sid, bool must_be_absent) {
  const auto path =
      LR"(Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\)" +
      sid;
  HKEY key = nullptr;
  const LSTATUS code =
      RegOpenKeyExW(HKEY_CURRENT_USER, path.c_str(), 0, KEY_QUERY_VALUE, &key);
  if (code == ERROR_FILE_NOT_FOUND) return;
  if (code) fail(L"inspect AppContainer mapping", code);
  std::vector<wchar_t> moniker(32768);
  DWORD bytes = static_cast<DWORD>(moniker.size() * sizeof(wchar_t));
  const LSTATUS query = RegGetValueW(key, nullptr, L"Moniker", RRF_RT_REG_SZ,
                                     nullptr, moniker.data(), &bytes);
  RegCloseKey(key);
  require(!must_be_absent && query == ERROR_SUCCESS &&
              _wcsicmp(profile.c_str(), moniker.data()) == 0,
          L"AppContainer registration changed; refusing profile mutation");
}
void grant_appcontainer_namespace(PSID sid, PSID write_sid) {
  wchar_t object_path[1024]{};
  ULONG object_length = 0;
  if (!GetAppContainerNamedObjectPath(nullptr, sid, 1024, object_path,
                                      &object_length))
    fail(L"AC object path");
  using OpenDirectory =
      NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES);
  auto open_directory = reinterpret_cast<OpenDirectory>(
      GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtOpenDirectoryObject"));
  DWORD session = 0;
  if (!ProcessIdToSessionId(GetCurrentProcessId(), &session)) fail(L"session");
  std::wstring absolute_objects =
      L"\\Sessions\\" + std::to_wstring(session) + L"\\" + object_path;
  UNICODE_STRING object_name{static_cast<USHORT>(absolute_objects.size() * 2),
                             static_cast<USHORT>(absolute_objects.size() * 2),
                             absolute_objects.data()};
  OBJECT_ATTRIBUTES directory_attributes{
      sizeof(directory_attributes), nullptr, &object_name,
      OBJ_CASE_INSENSITIVE,         nullptr, nullptr};
  Handle object_directory;
  NTSTATUS object_status =
      open_directory(&object_directory.value, READ_CONTROL | WRITE_DAC | 0xf,
                     &directory_attributes);
  if (object_status < 0)
    fail(L"open AC namespace", static_cast<DWORD>(object_status));
  PACL directory_acl = nullptr;
  PSECURITY_DESCRIPTOR directory_sd = nullptr;
  DWORD directory_code = GetSecurityInfo(
      object_directory.value, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
      nullptr, nullptr, &directory_acl, nullptr, &directory_sd);
  if (directory_code) fail(L"get AC namespace ACL", directory_code);
  EXPLICIT_ACCESSW directory_entry{};
  directory_entry.grfAccessPermissions = 0xf | READ_CONTROL;
  directory_entry.grfAccessMode = GRANT_ACCESS;
  directory_entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  directory_entry.Trustee.ptstrName = static_cast<LPWSTR>(write_sid);
  PACL directory_updated = nullptr;
  directory_code =
      SetEntriesInAclW(1, &directory_entry, directory_acl, &directory_updated);
  if (!directory_code)
    directory_code = SetSecurityInfo(object_directory.value, SE_KERNEL_OBJECT,
                                     DACL_SECURITY_INFORMATION, nullptr,
                                     nullptr, directory_updated, nullptr);
  LocalFree(directory_updated);
  LocalFree(directory_sd);
  if (directory_code) fail(L"grant AC namespace", directory_code);
}
}  // namespace latch
