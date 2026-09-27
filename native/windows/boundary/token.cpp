#include "token.h"
namespace latch {
Local parse_sid(const wchar_t* text) {
  PSID sid = nullptr;
  if (!ConvertStringSidToSidW(text, &sid)) fail(L"ConvertStringSidToSidW");
  return Local(sid);
}

std::wstring unique_sid_string() {
  UUID id{};
  const RPC_STATUS status = UuidCreate(&id);
  if (status != RPC_S_OK && status != RPC_S_UUID_LOCAL_ONLY)
    fail(L"UuidCreate", status);
  uint32_t tail[2]{};
  std::memcpy(tail, id.Data4, sizeof(tail));
  wchar_t value[128]{};
  std::swprintf(value, sizeof(value) / sizeof(value[0]),
                L"S-1-5-21-%lu-%lu-%lu-%lu", id.Data1,
                (static_cast<uint32_t>(id.Data2) << 16) | id.Data3, tail[0],
                tail[1]);
  return value;
}

std::vector<BYTE> token_info(HANDLE token, TOKEN_INFORMATION_CLASS type) {
  DWORD size = 0;
  GetTokenInformation(token, type, nullptr, 0, &size);
  if (size == 0) fail(L"GetTokenInformation(size)");
  std::vector<BYTE> result(size);
  if (!GetTokenInformation(token, type, result.data(), size, &size))
    fail(L"GetTokenInformation(data)");
  return result;
}

void prepare_default_dacl(HANDLE token, PSID sid) {
  const auto bytes = token_info(token, TokenDefaultDacl);
  const auto* info = reinterpret_cast<const TOKEN_DEFAULT_DACL*>(bytes.data());
  EXPLICIT_ACCESSW grant{};
  grant.grfAccessPermissions = GENERIC_ALL;
  grant.grfAccessMode = GRANT_ACCESS;
  grant.grfInheritance = NO_INHERITANCE;
  grant.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  grant.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
  grant.Trustee.ptstrName = static_cast<LPWSTR>(sid);
  PACL acl = nullptr;
  const DWORD code = SetEntriesInAclW(1, &grant, info->DefaultDacl, &acl);
  Local acl_owner(acl);
  if (code != ERROR_SUCCESS)
    fail(L"SetEntriesInAclW(token default DACL)", code);
  TOKEN_DEFAULT_DACL replacement{acl};
  if (!SetTokenInformation(token, TokenDefaultDacl, &replacement,
                           sizeof(replacement)))
    fail(L"SetTokenInformation(TokenDefaultDacl)");
}

Handle restrict_token(PSID write_sid) {
  Handle original, restricted;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &original.value))
    fail(L"open parent token");
  SID_AND_ATTRIBUTES restrictors[] = {{write_sid, 0}};
  Local admin = parse_sid(L"S-1-5-32-544");
  SID_AND_ATTRIBUTES disabled[] = {{admin.value, 0}};
  DWORD flags = WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE;
  DWORD restrictor_count = 1;
#ifdef LATCH_RECOVERY_TESTING
  wchar_t diagnostic[2]{};
  if (GetEnvironmentVariableW(L"LATCH_DIAG_NO_WRITE_RESTRICTION", diagnostic,
                              2)) {
    flags = DISABLE_MAX_PRIVILEGE;
    restrictor_count = 0;
  }
#endif
  if (!CreateRestrictedToken(
          original.value, flags, 1, disabled, 0, nullptr, restrictor_count,
          restrictor_count ? restrictors : nullptr, &restricted.value))
    fail(L"Restrict");
  prepare_default_dacl(restricted.value, write_sid);
  return restricted;
}
}  // namespace latch
