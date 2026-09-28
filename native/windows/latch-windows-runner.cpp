#define UNICODE
#define _UNICODE
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <aclapi.h>
#include <rpc.h>
#include <sddl.h>
#include <winternl.h>

#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cwctype>
#include <string>
#include <string_view>
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
  Local administrators = parse_sid(L"S-1-5-32-544");
  SID_AND_ATTRIBUTES restrictors[] = {{unique, 0}};
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
  if (!CreateRestrictedToken(original.value, WRITE_RESTRICTED, 1, disabled,
                             static_cast<DWORD>(removed.size()), removed.data(),
                             static_cast<DWORD>(std::size(restrictors)), restrictors,
                             &result.value)) fail(L"CreateRestrictedToken");
  prepare_token_object_acl(result.value, unique);
  prepare_default_dacl(result.value, unique);
  return result;
}

// MSYS creates this exact IPC directory before bash enters main. A write-restricted
// token cannot create it in BaseNamedObjects, so the parent creates and grants it.
struct MsysDirectory {
  Handle handle;
  PSID sid;
  explicit MsysDirectory(PSID value) : sid(value) {}
  MsysDirectory(const MsysDirectory&) = delete;
  MsysDirectory& operator=(const MsysDirectory&) = delete;
  MsysDirectory(MsysDirectory&&) = default;
  ~MsysDirectory() {
    if (handle.value == nullptr) return;
    PACL old_acl = nullptr;
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    if (GetSecurityInfo(handle.value, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                        nullptr, nullptr, &old_acl, nullptr, &descriptor) != ERROR_SUCCESS)
      return;
    Local descriptor_owner(descriptor);
    if (old_acl == nullptr) return;
    std::vector<BYTE> storage(old_acl->AclSize);
    auto* updated = reinterpret_cast<PACL>(storage.data());
    if (!InitializeAcl(updated, old_acl->AclSize, old_acl->AclRevision)) return;
    for (DWORD i = 0; i < old_acl->AceCount; ++i) {
      void* ace = nullptr;
      if (!GetAce(old_acl, i, &ace)) return;
      const auto* header = static_cast<const ACE_HEADER*>(ace);
      bool ours = false;
      if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) {
        const auto* entry = static_cast<const ACCESS_ALLOWED_ACE*>(ace);
        ours = EqualSid(const_cast<DWORD*>(&entry->SidStart), sid) != FALSE;
      }
      if (!ours && !AddAce(updated, old_acl->AclRevision, MAXDWORD, ace, header->AceSize))
        return;
    }
    SetSecurityInfo(handle.value, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                    nullptr, nullptr, updated, nullptr);
  }
};

std::wstring msys_ipc_name(const std::wstring& bash) {
  const size_t bin = bash.find_last_of(L"\\/");
  if (bin == std::wstring::npos) fail(L"MSYS bash path", ERROR_INVALID_NAME);
  const std::wstring root = bash.substr(0, bin);
  const size_t separator = root.find_last_of(L"\\/");
  if (separator == std::wstring::npos) fail(L"MSYS root path", ERROR_INVALID_NAME);
  std::wstring install = root.substr(0, separator);
  if (install.size() >= 4 &&
      _wcsicmp(install.c_str() + install.size() - 4, L"\\usr") == 0)
    install.resize(install.size() - 4);
  const std::wstring dll = install + L"\\usr\\bin\\msys-2.0.dll";
  Handle file(CreateFileW(dll.c_str(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE |
                            FILE_SHARE_DELETE, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL,
                          nullptr));
  if (file.value == INVALID_HANDLE_VALUE) fail(L"CreateFileW(msys-2.0.dll)");
  std::array<wchar_t, 32768> final_path{};
  const DWORD length = GetFinalPathNameByHandleW(file.value, final_path.data(),
                                                  static_cast<DWORD>(final_path.size()), 0);
  if (length < 5 || length >= final_path.size() || final_path[0] != L'\\' ||
      final_path[1] != L'\\' || final_path[2] != L'?')
    fail(L"GetFinalPathNameByHandleW(msys-2.0.dll)", ERROR_INVALID_NAME);
  final_path[1] = L'?';
  uint64_t hash = 0;
  for (DWORD i = 0; i < length; ++i)
    hash = static_cast<uint64_t>(std::towupper(final_path[i])) + hash * 65599;

  LARGE_INTEGER size{};
  if (!GetFileSizeEx(file.value, &size) || size.QuadPart <= 0 ||
      size.QuadPart > 16 * 1024 * 1024) fail(L"GetFileSizeEx(msys-2.0.dll)");
  std::vector<char> bytes(static_cast<size_t>(size.QuadPart));
  DWORD read = 0;
  if (!ReadFile(file.value, bytes.data(), static_cast<DWORD>(bytes.size()), &read, nullptr) ||
      read != bytes.size()) fail(L"ReadFile(msys-2.0.dll)");
  const std::string_view contents(bytes.data(), bytes.size());
  constexpr std::string_view prefix = "%%% MSYS shared id: ";
  const size_t start = contents.find(prefix);
  if (start == std::string_view::npos) fail(L"MSYS shared ID", ERROR_BAD_FORMAT);
  const size_t end = contents.find('\n', start);
  if (end == std::string_view::npos || end - start > 80)
    fail(L"MSYS shared ID", ERROR_BAD_FORMAT);
  const std::string shared_id(contents.substr(start + prefix.size(), end - start - prefix.size()));
  if (shared_id != "msys-2.0S5") fail(L"MSYS shared ID changed", ERROR_NOT_SUPPORTED);

  std::string build_date;
  for (size_t i = 0; i + 16 <= contents.size(); ++i) {
    const auto digit = [&](size_t offset) { return contents[i + offset] >= '0' &&
                                               contents[i + offset] <= '9'; };
    if (contents[i] != '2' || contents[i + 4] != '-' || contents[i + 7] != '-' ||
        contents[i + 10] != ' ' || contents[i + 13] != ':' ||
        !digit(1) || !digit(2) || !digit(3) || !digit(5) || !digit(6) ||
        !digit(8) || !digit(9) || !digit(11) || !digit(12) || !digit(14) ||
        !digit(15)) continue;
    const std::string candidate(contents.substr(i, 16));
    if (!build_date.empty() && build_date != candidate)
      fail(L"ambiguous MSYS build date", ERROR_BAD_FORMAT);
    build_date = candidate;
  }
  if (build_date.empty()) fail(L"MSYS build date", ERROR_BAD_FORMAT);
  wchar_t suffix[17]{};
  std::swprintf(suffix, std::size(suffix), L"%016llx",
                static_cast<unsigned long long>(hash));
  return L"\\BaseNamedObjects\\" +
      std::wstring(shared_id.begin(), shared_id.end()) +
      std::wstring(build_date.begin(), build_date.end()) + L"-" + suffix;
}

MsysDirectory prepare_msys_directory(const std::wstring& name, PSID sid) {
  using NtCreateDirectoryObjectFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK,
                                                      POBJECT_ATTRIBUTES);
  auto* create = reinterpret_cast<NtCreateDirectoryObjectFn>(
      GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateDirectoryObject"));
  if (create == nullptr) fail(L"NtCreateDirectoryObject lookup");
  UNICODE_STRING unicode{};
  unicode.Buffer = const_cast<wchar_t*>(name.data());
  unicode.Length = static_cast<USHORT>(name.size() * sizeof(wchar_t));
  unicode.MaximumLength = unicode.Length;
  OBJECT_ATTRIBUTES attributes{};
  attributes.Length = sizeof(attributes);
  attributes.ObjectName = &unicode;
  attributes.Attributes = OBJ_OPENIF;
  constexpr DWORD rights = 0x000f | READ_CONTROL;
  MsysDirectory directory(sid);
  const NTSTATUS status = create(&directory.handle.value, rights | WRITE_DAC, &attributes);
  if (status < 0) fail(L"NtCreateDirectoryObject(MSYS IPC)", static_cast<DWORD>(status));
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  DWORD code = GetSecurityInfo(directory.handle.value, SE_KERNEL_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr,
                               &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) fail(L"GetSecurityInfo(MSYS IPC)", code);
  EXPLICIT_ACCESSW grant{};
  grant.grfAccessPermissions = rights;
  grant.grfAccessMode = GRANT_ACCESS;
  grant.grfInheritance = NO_INHERITANCE;
  grant.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  grant.Trustee.ptstrName = static_cast<LPWSTR>(sid);
  PACL updated = nullptr;
  code = SetEntriesInAclW(1, &grant, old_acl, &updated);
  Local updated_owner(updated);
  if (code != ERROR_SUCCESS) fail(L"SetEntriesInAclW(MSYS IPC)", code);
  code = SetSecurityInfo(directory.handle.value, SE_KERNEL_OBJECT,
                         DACL_SECURITY_INFORMATION, nullptr, nullptr, updated, nullptr);
  if (code != ERROR_SUCCESS) fail(L"SetSecurityInfo(MSYS IPC)", code);
  return directory;
}

std::vector<MsysDirectory> prepare_msys_directories(const std::wstring& bash, PSID sid) {
  const std::wstring base = msys_ipc_name(bash);
  std::vector<MsysDirectory> directories;
  directories.push_back(prepare_msys_directory(base, sid));
  DWORD session = 0;
  if (!ProcessIdToSessionId(GetCurrentProcessId(), &session))
    fail(L"ProcessIdToSessionId");
  if (session != 0) {
    constexpr std::wstring_view prefix = L"\\BaseNamedObjects";
    const std::wstring session_name = L"\\Sessions\\BNOLINKS\\" +
        std::to_wstring(session) + base.substr(prefix.size());
    directories.push_back(prepare_msys_directory(session_name, sid));
  }
  return directories;
}

struct PrivateDesktop {
  HWINSTA station = nullptr;
  HDESK desktop = nullptr;
  std::wstring name;
  ~PrivateDesktop() {
    if (desktop != nullptr) CloseDesktop(desktop);
    if (station != nullptr) CloseWindowStation(station);
  }
  void create(PSID unique, HANDLE token, const std::wstring& unique_name) {
    (void)token;
    Handle parent_token;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &parent_token.value))
      fail(L"OpenProcessToken(private desktop)");
    const auto dacl_bytes = token_info(parent_token.value, TokenDefaultDacl);
    const auto* parent_dacl = reinterpret_cast<const TOKEN_DEFAULT_DACL*>(dacl_bytes.data());
    const std::wstring station_name = L"Latch-" + unique_name;
    const auto descriptor_for = [&](DWORD rights, PACL* acl, SECURITY_DESCRIPTOR* sd) {
      EXPLICIT_ACCESSW entry{};
      entry.grfAccessPermissions = rights;
      entry.grfAccessMode = GRANT_ACCESS;
      entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
      entry.Trustee.ptstrName = static_cast<LPWSTR>(unique);
      const DWORD code = SetEntriesInAclW(1, &entry, parent_dacl->DefaultDacl, acl);
      if (code != ERROR_SUCCESS) fail(L"SetEntriesInAclW(private desktop)", code);
      if (!InitializeSecurityDescriptor(sd, SECURITY_DESCRIPTOR_REVISION) ||
          !SetSecurityDescriptorDacl(sd, TRUE, *acl, FALSE))
        fail(L"private desktop security descriptor");
    };
    PACL station_acl = nullptr;
    SECURITY_DESCRIPTOR station_sd{};
    descriptor_for(WINSTA_ENUMDESKTOPS | WINSTA_READATTRIBUTES,
                    &station_acl, &station_sd);
    Local station_acl_owner(station_acl);
    SECURITY_ATTRIBUTES station_sa{sizeof(SECURITY_ATTRIBUTES), &station_sd, FALSE};
    station = CreateWindowStationW(station_name.c_str(), 0,
                                    WINSTA_ENUMDESKTOPS | WINSTA_READATTRIBUTES |
                                    WINSTA_CREATEDESKTOP | READ_CONTROL | WRITE_DAC,
                                    &station_sa);
    if (station == nullptr) fail(L"CreateWindowStationW(private)");
    HWINSTA previous = GetProcessWindowStation();
    if (!SetProcessWindowStation(station)) fail(L"SetProcessWindowStation(private)");
    PACL desktop_acl = nullptr;
    SECURITY_DESCRIPTOR desktop_sd{};
    descriptor_for(DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_CREATEWINDOW,
                    &desktop_acl, &desktop_sd);
    Local desktop_acl_owner(desktop_acl);
    SECURITY_ATTRIBUTES desktop_sa{sizeof(SECURITY_ATTRIBUTES), &desktop_sd, FALSE};
    desktop = CreateDesktopW(L"Sandbox", nullptr, nullptr, 0,
                              DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS |
                              DESKTOP_CREATEWINDOW | READ_CONTROL | WRITE_DAC,
                              &desktop_sa);
    const DWORD desktop_error = GetLastError();
    if (!SetProcessWindowStation(previous)) fail(L"restore process window station");
    if (desktop == nullptr) fail(L"CreateDesktopW(private)", desktop_error);
    name = station_name + L"\\Sandbox";
  }
};

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
  PrivateDesktop private_desktop;
  if (GetEnvironmentVariableW(L"LATCH_PRIVATE_DESKTOP_PROBE", nullptr, 0))
    private_desktop.create(unique.value, token.value, unique_name);
  auto msys_directories = prepare_msys_directories(bash, unique.value);
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
  if (!private_desktop.name.empty()) startup.lpDesktop = private_desktop.name.data();
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
