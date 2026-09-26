#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <aclapi.h>
#include <rpc.h>
#include <sddl.h>

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

struct Error {
  const wchar_t* api;
  DWORD code;
};

[[noreturn]] void fail(const wchar_t* api, DWORD code = GetLastError()) {
  throw Error{api, code};
}

struct Handle {
  HANDLE value = nullptr;
  ~Handle() { if (value != nullptr && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
  Handle() = default;
  explicit Handle(HANDLE input) : value(input) {}
  Handle(const Handle&) = delete;
  Handle& operator=(const Handle&) = delete;
  Handle(Handle&& other) noexcept : value(other.value) { other.value = nullptr; }
  Handle& operator=(Handle&&) = delete;
};

struct Local {
  void* value = nullptr;
  ~Local() { if (value != nullptr) LocalFree(value); }
  Local() = default;
  explicit Local(void* input) : value(input) {}
  Local(const Local&) = delete;
  Local& operator=(const Local&) = delete;
};

Local parse_sid(const wchar_t* text) {
  PSID sid = nullptr;
  if (!ConvertStringSidToSidW(text, &sid)) fail(L"ConvertStringSidToSidW");
  return Local(sid);
}

std::wstring unique_sid_string() {
  UUID id{};
  const RPC_STATUS status = UuidCreate(&id);
  if (status != RPC_S_OK && status != RPC_S_UUID_LOCAL_ONLY) fail(L"UuidCreate", status);
  uint32_t tail[2]{};
  std::memcpy(tail, id.Data4, sizeof(tail));
  wchar_t value[128]{};
  std::swprintf(value, sizeof(value) / sizeof(value[0]), L"S-1-5-21-%lu-%lu-%lu-%lu",
               id.Data1, (static_cast<uint32_t>(id.Data2) << 16) | id.Data3,
               tail[0], tail[1]);
  return value;
}

std::wstring quote(const std::wstring& value) {
  std::wstring result = L"\"";
  size_t slashes = 0;
  for (wchar_t character : value) {
    if (character == L'\\') { ++slashes; continue; }
    if (character == L'"') {
      result.append(slashes * 2 + 1, L'\\');
      result.push_back(L'"');
    } else {
      result.append(slashes, L'\\');
      result.push_back(character);
    }
    slashes = 0;
  }
  result.append(slashes * 2, L'\\');
  result.push_back(L'"');
  return result;
}

DWORD update_acl(const std::wstring& path, PSID sid, ACCESS_MODE mode,
                 DWORD rights, DWORD inheritance) {
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  DWORD code = GetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
                                     DACL_SECURITY_INFORMATION, nullptr, nullptr,
                                     &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) return code;
  EXPLICIT_ACCESSW entry{};
  entry.grfAccessPermissions = rights;
  entry.grfAccessMode = mode;
  entry.grfInheritance = inheritance;
  entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
  entry.Trustee.ptstrName = static_cast<LPWSTR>(sid);
  PACL new_acl = nullptr;
  code = SetEntriesInAclW(1, &entry, old_acl, &new_acl);
  Local new_acl_owner(new_acl);
  if (code != ERROR_SUCCESS) return code;
  return SetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr, new_acl, nullptr);
}

DWORD remove_sid_aces(const std::wstring& path, PSID sid) {
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  DWORD code = GetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
                                     DACL_SECURITY_INFORMATION, nullptr, nullptr,
                                     &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) return code;
  if (old_acl == nullptr) return ERROR_INVALID_ACL;
  std::vector<BYTE> storage(old_acl->AclSize);
  auto* updated = reinterpret_cast<PACL>(storage.data());
  if (!InitializeAcl(updated, old_acl->AclSize, old_acl->AclRevision)) return GetLastError();
  for (DWORD i = 0; i < old_acl->AceCount; ++i) {
    void* ace = nullptr;
    if (!GetAce(old_acl, i, &ace)) return GetLastError();
    const auto* header = static_cast<const ACE_HEADER*>(ace);
    bool ours = false;
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE ||
        header->AceType == ACCESS_DENIED_ACE_TYPE) {
      const auto* entry = static_cast<const ACCESS_ALLOWED_ACE*>(ace);
      ours = EqualSid(const_cast<DWORD*>(&entry->SidStart), sid) != FALSE;
    }
    if (!ours && !AddAce(updated, old_acl->AclRevision, MAXDWORD, ace, header->AceSize))
      return GetLastError();
  }
  return SetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr, updated, nullptr);
}

class Grants {
 public:
  explicit Grants(PSID sid) : sid_(sid) {}
  ~Grants() { cleanup(); }
  void add(const std::wstring& path, ACCESS_MODE mode, DWORD rights) {
    const DWORD code = update_acl(path, sid_, mode, rights,
                                  SUB_CONTAINERS_AND_OBJECTS_INHERIT);
    if (code != ERROR_SUCCESS) fail(L"SetNamedSecurityInfoW(grant)", code);
    paths_.push_back(path);
  }
  void cleanup() noexcept {
    for (auto it = paths_.rbegin(); it != paths_.rend(); ++it) {
      const DWORD code = remove_sid_aces(*it, sid_);
      if (code != ERROR_SUCCESS)
        std::fwprintf(stderr, L"windows runner: ACL revoke failed (%lu)\n", code);
    }
    paths_.clear();
  }

 private:
  PSID sid_;
  std::vector<std::wstring> paths_;
};

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
  if (code != ERROR_SUCCESS) fail(L"SetEntriesInAclW(token default DACL)", code);
  TOKEN_DEFAULT_DACL replacement{acl};
  if (!SetTokenInformation(token, TokenDefaultDacl, &replacement, sizeof(replacement)))
    fail(L"SetTokenInformation(TokenDefaultDacl)");
}

void prepare_token_object_acl(HANDLE token, PSID sid) {
  PACL original = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  DWORD code = GetSecurityInfo(token, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                               nullptr, nullptr, &original, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) fail(L"GetSecurityInfo(token)", code);
  EXPLICIT_ACCESSW grant{};
  grant.grfAccessPermissions = TOKEN_ALL_ACCESS;
  grant.grfAccessMode = GRANT_ACCESS;
  grant.grfInheritance = NO_INHERITANCE;
  grant.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  grant.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
  grant.Trustee.ptstrName = static_cast<LPWSTR>(sid);
  PACL updated = nullptr;
  code = SetEntriesInAclW(1, &grant, original, &updated);
  Local updated_owner(updated);
  if (code != ERROR_SUCCESS) fail(L"SetEntriesInAclW(token)", code);
  code = SetSecurityInfo(token, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                         nullptr, nullptr, updated, nullptr);
  if (code != ERROR_SUCCESS) fail(L"SetSecurityInfo(token)", code);
}

Handle restricted_token(PSID unique) {
  Handle original;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY |
                        TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | READ_CONTROL | WRITE_DAC,
                        &original.value)) fail(L"OpenProcessToken");
  const auto groups_bytes = token_info(original.value, TokenGroups);
  const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(groups_bytes.data());
  PSID logon = nullptr;
  for (DWORD i = 0; i < groups->GroupCount; ++i) {
    if ((groups->Groups[i].Attributes & SE_GROUP_LOGON_ID) == SE_GROUP_LOGON_ID)
      logon = groups->Groups[i].Sid;
  }
  if (logon == nullptr) fail(L"TokenGroups(logon SID)", ERROR_NOT_FOUND);
  Local users = parse_sid(L"S-1-5-32-545");
  Local everyone = parse_sid(L"S-1-1-0");
  Local restricted_code = parse_sid(L"S-1-5-12");
  Local administrators = parse_sid(L"S-1-5-32-544");
  SID_AND_ATTRIBUTES restrictors[] = {{users.value, 0}, {everyone.value, 0},
                                       {restricted_code.value, 0}, {logon, 0}, {unique, 0}};
  SID_AND_ATTRIBUTES disabled[] = {{administrators.value, 0}};
  LUID traverse{};
  if (!LookupPrivilegeValueW(nullptr, SE_CHANGE_NOTIFY_NAME, &traverse))
    fail(L"LookupPrivilegeValueW(SeChangeNotifyPrivilege)");
  const auto privileges_bytes = token_info(original.value, TokenPrivileges);
  const auto* privileges = reinterpret_cast<const TOKEN_PRIVILEGES*>(privileges_bytes.data());
  std::vector<LUID_AND_ATTRIBUTES> removed;
  for (DWORD i = 0; i < privileges->PrivilegeCount; ++i) {
    const auto entry = privileges->Privileges[i];
    if (entry.Luid.LowPart != traverse.LowPart || entry.Luid.HighPart != traverse.HighPart)
      removed.push_back(entry);
  }
  Handle result;
  if (!CreateRestrictedToken(original.value, 0, 1, disabled,
                             static_cast<DWORD>(removed.size()), removed.data(),
                             static_cast<DWORD>(std::size(restrictors)), restrictors,
                             &result.value)) fail(L"CreateRestrictedToken");
  prepare_token_object_acl(result.value, unique);
  prepare_default_dacl(result.value, unique);
  return result;
}

int run(int argc, wchar_t** argv) {
  if (argc < 6 || (std::wcscmp(argv[4], L"read") != 0 &&
                   std::wcscmp(argv[4], L"write") != 0)) {
    std::fwprintf(stderr, L"usage: latch-windows-runner <guard> <bash> <workspace> <read|write> <command> [--read-root path] [--write-root path] [--deny-read-root path] [--git-write]\n");
    return 2;
  }
  const std::wstring guard = argv[1];
  const std::wstring bash = argv[2];
  const std::wstring workspace = argv[3];
  const bool writable = std::wcscmp(argv[4], L"write") == 0;
  const std::wstring script = argv[5];
  if (GetFileAttributesW(guard.c_str()) == INVALID_FILE_ATTRIBUTES ||
      GetFileAttributesW(bash.c_str()) == INVALID_FILE_ATTRIBUTES)
    fail(L"GetFileAttributesW(runtime)");
  const std::wstring unique_name = unique_sid_string();
  Local unique = parse_sid(unique_name.c_str());
  Grants grants(unique.value);
  constexpr DWORD read_rights = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
  constexpr DWORD write_rights = FILE_WRITE_DATA | FILE_APPEND_DATA |
      FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES | DELETE | FILE_DELETE_CHILD |
      WRITE_DAC | WRITE_OWNER;
  grants.add(workspace, GRANT_ACCESS,
             writable ? read_rights | write_rights : read_rights);
  if (!writable) grants.add(workspace, DENY_ACCESS, write_rights);
  bool git_write = false;
  for (int i = 6; i < argc; ++i) {
    if (std::wcscmp(argv[i], L"--git-write") == 0) { git_write = true; continue; }
    if (i + 1 >= argc) return 2;
    const std::wstring option = argv[i++];
    const std::wstring path = argv[i];
    if (option == L"--read-root") grants.add(path, GRANT_ACCESS, read_rights);
    else if (option == L"--write-root") grants.add(path, GRANT_ACCESS,
                                                  read_rights | write_rights);
    else if (option == L"--deny-read-root") grants.add(path, DENY_ACCESS,
                                                      read_rights | write_rights);
    else return 2;
  }
  if (writable && !git_write) {
    const std::wstring git = workspace + L"\\.git";
    if (GetFileAttributesW(git.c_str()) != INVALID_FILE_ATTRIBUTES)
      grants.add(git, DENY_ACCESS, write_rights);
  }
  Handle token = restricted_token(unique.value);
  Handle job(CreateJobObjectW(nullptr, nullptr));
  if (job.value == nullptr) fail(L"CreateJobObjectW");
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  if (!SetInformationJobObject(job.value, JobObjectExtendedLimitInformation,
                               &limits, sizeof(limits))) fail(L"SetInformationJobObject");
  const std::wstring line = quote(guard) + L" -- " + quote(bash) +
                            L" --noprofile --norc -c " + quote(script);
  std::vector<wchar_t> command(line.begin(), line.end());
  command.push_back(0);
  STARTUPINFOW startup{};
  startup.cb = sizeof(startup);
  startup.dwFlags = STARTF_USESTDHANDLES;
  startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
  startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
  startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
  PROCESS_INFORMATION child{};
  if (!CreateProcessAsUserW(token.value, guard.c_str(), command.data(), nullptr,
                            nullptr, TRUE, CREATE_SUSPENDED | CREATE_NO_WINDOW,
                            nullptr, workspace.c_str(), &startup, &child))
    fail(L"CreateProcessAsUserW");
  Handle child_process(child.hProcess);
  Handle child_thread(child.hThread);
  if (!AssignProcessToJobObject(job.value, child_process.value)) {
    TerminateProcess(child_process.value, 125);
    fail(L"AssignProcessToJobObject");
  }
  if (ResumeThread(child_thread.value) == static_cast<DWORD>(-1))
    fail(L"ResumeThread");
  if (WaitForSingleObject(child_process.value, INFINITE) != WAIT_OBJECT_0)
    fail(L"WaitForSingleObject");
  DWORD exit_code = 0;
  if (!GetExitCodeProcess(child_process.value, &exit_code)) fail(L"GetExitCodeProcess");
  grants.cleanup();
  return static_cast<int>(exit_code);
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
  try { return run(argc, argv); }
  catch (const Error& error) {
    std::fwprintf(stderr, L"windows runner: %ls failed (Win32 %lu)\n",
                  error.api, error.code);
    return 125;
  }
}
