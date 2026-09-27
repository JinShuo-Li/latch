#include "desktop.h"

#include "token.h"
namespace latch {
PrivateDesktop::~PrivateDesktop() {
  if (desktop) CloseDesktop(desktop);
}
void PrivateDesktop::create(PSID unique, const std::wstring& unique_name) {
  Handle parent_token;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &parent_token.value))
    fail(L"OpenProcessToken(private desktop)");
  const auto dacl_bytes = token_info(parent_token.value, TokenDefaultDacl);
  const auto* parent_dacl =
      reinterpret_cast<const TOKEN_DEFAULT_DACL*>(dacl_bytes.data());
  const std::wstring station_name = L"Latch-" + unique_name;
  const auto descriptor_for = [&](DWORD rights, PACL* acl,
                                  SECURITY_DESCRIPTOR* sd) {
    EXPLICIT_ACCESSW entry{};
    entry.grfAccessPermissions = rights;
    entry.grfAccessMode = GRANT_ACCESS;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.ptstrName = static_cast<LPWSTR>(unique);
    const DWORD code =
        SetEntriesInAclW(1, &entry, parent_dacl->DefaultDacl, acl);
    if (code != ERROR_SUCCESS) fail(L"SetEntriesInAclW(private desktop)", code);
    if (!InitializeSecurityDescriptor(sd, SECURITY_DESCRIPTOR_REVISION) ||
        !SetSecurityDescriptorDacl(sd, TRUE, *acl, FALSE))
      fail(L"private desktop security descriptor");
  };
  PACL desktop_acl = nullptr;
  SECURITY_DESCRIPTOR desktop_sd{};
  descriptor_for(
      DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_CREATEWINDOW,
      &desktop_acl, &desktop_sd);
  Local desktop_acl_owner(desktop_acl);
  SECURITY_ATTRIBUTES desktop_sa{sizeof(SECURITY_ATTRIBUTES), &desktop_sd,
                                 FALSE};
  desktop = CreateDesktopW(station_name.c_str(), nullptr, nullptr, 0,
                           DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS |
                               DESKTOP_CREATEWINDOW | READ_CONTROL | WRITE_DAC,
                           &desktop_sa);
  const DWORD desktop_error = GetLastError();

  if (desktop == nullptr) fail(L"CreateDesktopW(private)", desktop_error);
  // CreateDesktopW uses this process's window station. Hosted CI runners can
  // use a noninteractive station, so never assume that it is WinSta0.
  wchar_t actual_station[256]{};
  DWORD station_bytes = 0;
  const HWINSTA station = GetProcessWindowStation();
  if (!station || !GetUserObjectInformationW(station, UOI_NAME,
                                             actual_station,
                                             sizeof(actual_station),
                                             &station_bytes))
    fail(L"query parent window station");
  name = std::wstring(actual_station) + L"\\" + station_name;
}

void PrivateDesktop::grant_package(PSID sid) {
  PACL existing = nullptr;
  PSECURITY_DESCRIPTOR sd = nullptr;
  const DWORD read_code =
      GetSecurityInfo(desktop, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION,
                      nullptr, nullptr, &existing, nullptr, &sd);
  Local descriptor_owner(sd);
  if (read_code) fail(L"read desktop ACL", read_code);
  EXPLICIT_ACCESSW e{};
  e.grfAccessPermissions =
      DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_CREATEWINDOW;
  e.grfAccessMode = GRANT_ACCESS;
  e.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  e.Trustee.ptstrName = static_cast<LPWSTR>(sid);
  PACL updated = nullptr;
  DWORD code = SetEntriesInAclW(1, &e, existing, &updated);
  if (code == 0)
    code = SetSecurityInfo(desktop, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION,
                           nullptr, nullptr, updated, nullptr);
  LocalFree(updated);
  if (code) fail(L"desktop AC", code);
}
}  // namespace latch
