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
#include <filesystem>
#include <algorithm>
#include <set>
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
  if (mode == DENY_ACCESS) {
    // A protected DACL can contain an explicit allow before a converted
    // inherited deny. SetEntriesInAcl may merge at that old position. Our
    // deny must precede all allows, without changing their relative order.
    const DWORD bytes = (old_acl ? old_acl->AclSize : sizeof(ACL)) +
        sizeof(ACCESS_DENIED_ACE) - sizeof(DWORD) + GetLengthSid(sid);
    if (bytes > MAXWORD) return ERROR_ALLOTTED_SPACE_EXCEEDED;
    std::vector<BYTE> storage(bytes);
    auto* updated = reinterpret_cast<PACL>(storage.data());
    if (!InitializeAcl(updated, bytes, ACL_REVISION_DS) ||
        !AddAccessDeniedAceEx(updated, ACL_REVISION_DS, inheritance, rights, sid))
      return GetLastError();
    if (old_acl) {
      for (DWORD i = 0; i < old_acl->AceCount; ++i) {
        void* ace = nullptr;
        if (!GetAce(old_acl, i, &ace)) return GetLastError();
        const auto* header = static_cast<const ACE_HEADER*>(ace);
        const auto* denied = static_cast<const ACCESS_DENIED_ACE*>(ace);
        if (header->AceType == ACCESS_DENIED_ACE_TYPE &&
            header->AceFlags == inheritance && denied->Mask == rights &&
            EqualSid(const_cast<DWORD*>(&denied->SidStart), sid)) continue;
        if (!AddAce(updated, ACL_REVISION_DS, MAXDWORD, ace, header->AceSize))
          return GetLastError();
      }
    }
    return SetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION, nullptr, nullptr, updated, nullptr);
  }
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
    if (mode == DENY_ACCESS) {
      // Inheritance alone never protects a child with SE_DACL_PROTECTED.
      // Apply the deny to each existing object, including protected children.
      const auto resolved = std::filesystem::canonical(path).wstring();
      if (!denied_.insert(resolved).second) return;
      const DWORD attributes = GetFileAttributesW(resolved.c_str());
      if (attributes == INVALID_FILE_ATTRIBUTES) fail(L"deny attributes");
      if (attributes & FILE_ATTRIBUTE_DIRECTORY) {
        for (const auto& child : std::filesystem::directory_iterator(resolved))
          add(child.path().wstring(), mode, rights);
      }
      add_one(resolved, mode, rights);
      return;
    }
    add_one(path, mode, rights);
  }
  void add_one(const std::wstring& path, ACCESS_MODE mode, DWORD rights) {
    const DWORD attributes = GetFileAttributesW(path.c_str());
    if (attributes == INVALID_FILE_ATTRIBUTES) fail(L"grant attributes");
    const DWORD inheritance = (attributes & FILE_ATTRIBUTE_DIRECTORY) ?
        SUB_CONTAINERS_AND_OBJECTS_INHERIT : NO_INHERITANCE;
    const DWORD code = update_acl(path, sid_, mode, rights,
                                  inheritance);
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
  std::set<std::wstring> denied_;
};

// A write grant changes an NTFS object's ACL, which is shared by all hardlinks.
// Validate every link before granting a root; a name outside the approved roots
// must never obtain a write grant through an alias in the workspace.
void validate_write_tree(const std::filesystem::path& path,
                         const std::vector<std::wstring>& allowed,
                         std::vector<Handle>& locks) {
  Handle object(CreateFileW(path.c_str(), FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (object.value == INVALID_HANDLE_VALUE) fail(L"open write-grant object");
  BY_HANDLE_FILE_INFORMATION information{};
  if (!GetFileInformationByHandle(object.value, &information)) fail(L"write-grant identity");
  locks.push_back(std::move(object));
  if (information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) return;
  if (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      validate_write_tree(child.path(), allowed, locks);
    return;
  }
  if (information.nNumberOfLinks <= 1) return;
  wchar_t volume[32768]{};
  if (!GetVolumePathNameW(path.c_str(), volume, 32768)) fail(L"hardlink volume");
  std::vector<wchar_t> name(32768);
  DWORD length = static_cast<DWORD>(name.size());
  HANDLE iterator = FindFirstFileNameW(path.c_str(), 0, &length, name.data());
  if (iterator == INVALID_HANDLE_VALUE) fail(L"enumerate hardlinks");
  bool safe = true;
  DWORD error = ERROR_SUCCESS;
  do {
    auto link = std::filesystem::path(volume) / std::filesystem::path(name.data()).relative_path();
    const auto canonical = std::filesystem::canonical(link).wstring();
    safe = std::any_of(allowed.begin(), allowed.end(), [&](const std::wstring& root) {
      return canonical.size() > root.size() &&
          _wcsnicmp(canonical.c_str(), root.c_str(), root.size()) == 0 &&
          std::filesystem::path::preferred_separator == canonical[root.size()];
    });
    if (!safe) break;
    length = static_cast<DWORD>(name.size());
  } while (FindNextFileNameW(iterator, &length, name.data()));
  if (safe) error = GetLastError();
  FindClose(iterator);
  if (!safe) fail(L"write root contains an outside hardlink", ERROR_ACCESS_DENIED);
  if (error != ERROR_HANDLE_EOF) fail(L"enumerate remaining hardlinks", error);
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

void protect_sensitive_tree(const std::filesystem::path& input, std::set<std::wstring>& visited) {
  const auto path = std::filesystem::canonical(input).wstring();
  if (!visited.insert(path).second) return;
  PACL acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  DWORD code = GetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
      DACL_SECURITY_INFORMATION, nullptr, nullptr, &acl, nullptr, &descriptor);
  Local owner(descriptor);
  if (code) fail(L"read sensitive ACL", code);
  if (!acl) fail(L"sensitive path has a NULL DACL", ERROR_INVALID_ACL);
  std::vector<BYTE> storage(acl->AclSize);
  auto* updated = reinterpret_cast<PACL>(storage.data());
  if (!InitializeAcl(updated, acl->AclSize, acl->AclRevision)) fail(L"initialize sensitive ACL");
  const SID_IDENTIFIER_AUTHORITY package_authority = SECURITY_APP_PACKAGE_AUTHORITY;
  for (DWORD i = 0; i < acl->AceCount; ++i) {
    void* ace = nullptr;
    if (!GetAce(acl, i, &ace)) fail(L"read sensitive ACE");
    auto* header = static_cast<ACE_HEADER*>(ace);
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) {
      const auto* allowed = static_cast<const ACCESS_ALLOWED_ACE*>(ace);
      PSID principal = const_cast<DWORD*>(&allowed->SidStart);
      if (std::memcmp(GetSidIdentifierAuthority(principal), &package_authority,
                       sizeof(package_authority)) == 0) continue;
    } else if (header->AceType != ACCESS_DENIED_ACE_TYPE) {
      fail(L"unsupported sensitive ACL entry", ERROR_INVALID_ACL);
    }
    // Preserve existing host rights explicitly while sealing inheritance.
    header->AceFlags &= ~INHERITED_ACE;
    if (!AddAce(updated, acl->AclRevision, MAXDWORD, ace, header->AceSize))
      fail(L"copy sensitive ACE");
  }
  code = SetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
      DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
      nullptr, nullptr, updated, nullptr);
  if (code) fail(L"seal sensitive ACL", code);
  if (std::filesystem::is_directory(path)) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      protect_sensitive_tree(child.path(), visited);
  }
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

    PACL desktop_acl = nullptr;
    SECURITY_DESCRIPTOR desktop_sd{};
    descriptor_for(DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_CREATEWINDOW,
                    &desktop_acl, &desktop_sd);
    Local desktop_acl_owner(desktop_acl);
    SECURITY_ATTRIBUTES desktop_sa{sizeof(SECURITY_ATTRIBUTES), &desktop_sd, FALSE};
    desktop = CreateDesktopW(station_name.c_str(), nullptr, nullptr, 0,
                              DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS |
                              DESKTOP_CREATEWINDOW | READ_CONTROL | WRITE_DAC,
                              &desktop_sa);
    const DWORD desktop_error = GetLastError();

    if (desktop == nullptr) fail(L"CreateDesktopW(private)", desktop_error);
    name = L"WinSta0\\" + station_name;
  }
};

struct GitReservation {
  Handle handle;
  void create(const std::filesystem::path& workspace) {
    const auto marker = workspace / L".git";
    if (GetFileAttributesW(marker.c_str()) != INVALID_FILE_ATTRIBUTES) return;
    if (GetLastError() != ERROR_FILE_NOT_FOUND) fail(L"inspect .git reservation");
    // A file without delete sharing reserves the metadata name. The empty
    // file is removed after the complete process tree exits.
    handle.value = CreateFileW(marker.c_str(), GENERIC_READ | DELETE, FILE_SHARE_READ,
        nullptr, CREATE_NEW, FILE_ATTRIBUTE_HIDDEN | FILE_FLAG_DELETE_ON_CLOSE, nullptr);
    if (handle.value == INVALID_HANDLE_VALUE) { handle.value = nullptr; fail(L"reserve .git name"); }
  }
};

} // namespace
#include <userenv.h>
#include <objbase.h>
#include "detours.h"
#include "handles.h"
int wmain(int argc, wchar_t** argv) {
  if (argc < 5) return 2;
  SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
  PSID sid = nullptr;
  const std::wstring name = L"LatchProbe." + unique_sid_string();
  HRESULT hr = CreateAppContainerProfile(name.c_str(), name.c_str(), L"Disposable Latch probe", nullptr, 0, &sid);
  if (FAILED(hr)) { std::fwprintf(stderr,L"CreateAppContainerProfile %lx\n",hr); return 125; }
  int result=125;
  try {
    Grants grants(sid);
    // Remove AppContainer read grants on sensitive paths; package-specific
    // deny ACEs do not suppress the All Application Packages allow route.
    std::set<std::wstring> protected_paths;
    GitReservation git_reservation;
    DWORD timeout_ms = INFINITE;


    constexpr DWORD read_rights = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    constexpr DWORD write_rights = FILE_GENERIC_WRITE | DELETE;
    const bool writable = std::wcscmp(argv[4], L"write") == 0;
    std::vector<std::wstring> write_roots;
    if (writable) write_roots.push_back(std::filesystem::canonical(argv[1]).wstring());
    for (int i = 5; i < argc; i += 2) {
      if (i + 1 >= argc) fail(L"missing option path", ERROR_INVALID_PARAMETER);
      if (std::wcscmp(argv[i], L"--write-root") == 0)
        write_roots.push_back(std::filesystem::canonical(argv[i + 1]).wstring());
    }
    std::vector<Handle> grant_locks;
    for (const auto& root : write_roots) validate_write_tree(root, write_roots, grant_locks);
    grants.add(argv[1], GRANT_ACCESS, read_rights | (writable ? write_rights : 0));
    SIZE_T size=0;
    InitializeProcThreadAttributeList(nullptr,3,0,&size);
    std::vector<BYTE> buffer(size);
    auto* attrs=reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(buffer.data());
    if (!InitializeProcThreadAttributeList(attrs,3,0,&size)) fail(L"Initialize attrs");
    Local internet = parse_sid(L"S-1-15-3-1");
    Local private_network = parse_sid(L"S-1-15-3-3");
    SID_AND_ATTRIBUTES network_caps[] = {{internet.value, SE_GROUP_ENABLED},
                                         {private_network.value, SE_GROUP_ENABLED}};
    SECURITY_CAPABILITIES caps{}; caps.AppContainerSid=sid;
    for (int i = 5; i + 1 < argc; i += 2) {
      if (std::wcscmp(argv[i], L"--network") == 0 && std::wcscmp(argv[i + 1], L"yes") == 0) {
        caps.Capabilities = network_caps;
        caps.CapabilityCount = 2;
      }
    }
    if (!UpdateProcThreadAttribute(attrs,0,PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,&caps,sizeof(caps),nullptr,nullptr)) fail(L"Update attrs");
    Handle original, restricted;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &original.value)) fail(L"Open token");
    Local write_sid=parse_sid(unique_sid_string().c_str());
    Grants write_grants(write_sid.value);
    // Windows redirects GetTempPath inside an AppContainer to this private
    // per-call directory, regardless of TEMP/TMP. Grant only this scratch tree.
    LPWSTR sid_text = nullptr;
    if (!ConvertSidToStringSidW(sid, &sid_text)) fail(L"AC SID text");
    Local sid_text_owner(sid_text);
    LPWSTR package_path = nullptr;
    const HRESULT folder_result = GetAppContainerFolderPath(sid_text, &package_path);
    if (FAILED(folder_result)) fail(L"AC scratch folder", folder_result);
    const std::filesystem::path scratch = std::filesystem::path(package_path) / L"Temp";
    CoTaskMemFree(package_path);
    std::filesystem::create_directories(scratch);
    grants.add(scratch.wstring(), GRANT_ACCESS, read_rights | write_rights);
    write_grants.add(scratch.wstring(), GRANT_ACCESS, read_rights | write_rights);
    write_grants.add(argv[1], GRANT_ACCESS, read_rights | (writable ? write_rights : 0));
    for(int i=5;i<argc;++i) {
      const std::wstring option=argv[i++];
      if(i>=argc) fail(L"missing option path",ERROR_INVALID_PARAMETER);
      if(option==L"--read-root") grants.add(argv[i],GRANT_ACCESS,read_rights);
      else if(option==L"--network") {
        if (std::wcscmp(argv[i], L"yes") != 0 && std::wcscmp(argv[i], L"no") != 0)
          fail(L"invalid network mode", ERROR_INVALID_PARAMETER);
      }
      else if(option==L"--timeout-ms") {
        wchar_t* end = nullptr;
        const unsigned long value = std::wcstoul(argv[i], &end, 10);
        if (!end || *end || value == 0 || value >= INFINITE)
          fail(L"invalid timeout", ERROR_INVALID_PARAMETER);
        timeout_ms = value;
      }
      else if(option==L"--protect-git") {
        git_reservation.create(argv[i]);
      }
      else if(option==L"--write-root") {
        grants.add(argv[i],GRANT_ACCESS,read_rights|write_rights);
        write_grants.add(argv[i],GRANT_ACCESS,read_rights|write_rights);
      } else if(option==L"--deny") {
        protect_sensitive_tree(argv[i], protected_paths);
        grants.add(argv[i],DENY_ACCESS,FILE_ALL_ACCESS);
        write_grants.add(argv[i],DENY_ACCESS,FILE_ALL_ACCESS);
      } else if(option==L"--deny-write") {
        constexpr DWORD mutate = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA |
            FILE_WRITE_ATTRIBUTES | DELETE | WRITE_DAC | WRITE_OWNER | FILE_DELETE_CHILD;
        grants.add(argv[i],DENY_ACCESS,mutate);
        write_grants.add(argv[i],DENY_ACCESS,mutate);
      } else fail(L"invalid option",ERROR_INVALID_PARAMETER);
    }
    // Existing objects cannot be renamed or hardlinked between validation
    // and grant propagation. Release these pins before developer commands.
    grant_locks.clear();
    SID_AND_ATTRIBUTES restrictors[]={{write_sid.value,0}};
    Local admin=parse_sid(L"S-1-5-32-544");
    SID_AND_ATTRIBUTES disabled[]={{admin.value,0}};
    if (!CreateRestrictedToken(original.value, WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE, 1, disabled, 0, nullptr, 1, restrictors, &restricted.value)) fail(L"Restrict");
    prepare_default_dacl(restricted.value,write_sid.value);
    PrivateDesktop desktop; desktop.create(write_sid.value,restricted.value,unique_sid_string());
    {
      PACL existing=nullptr; PSECURITY_DESCRIPTOR sd=nullptr;
      GetSecurityInfo(desktop.desktop,SE_WINDOW_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&existing,nullptr,&sd);
      EXPLICIT_ACCESSW e{}; e.grfAccessPermissions=DESKTOP_READOBJECTS|DESKTOP_WRITEOBJECTS|DESKTOP_CREATEWINDOW;
      e.grfAccessMode=GRANT_ACCESS; e.Trustee.TrusteeForm=TRUSTEE_IS_SID; e.Trustee.ptstrName=static_cast<LPWSTR>(sid);
      PACL updated=nullptr; DWORD code=SetEntriesInAclW(1,&e,existing,&updated);
      if(code==0) code=SetSecurityInfo(desktop.desktop,SE_WINDOW_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,updated,nullptr);
      LocalFree(updated); LocalFree(sd); if(code) fail(L"desktop AC",code);
    }
    STARTUPINFOEXW si{}; si.StartupInfo.cb=sizeof(si); si.lpAttributeList=attrs; si.StartupInfo.lpDesktop=desktop.name.data();
    si.StartupInfo.dwFlags=STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput=GetStdHandle(STD_INPUT_HANDLE);
    si.StartupInfo.hStdOutput=GetStdHandle(STD_OUTPUT_HANDLE);
    si.StartupInfo.hStdError=GetStdHandle(STD_ERROR_HANDLE);
    std::wstring line=quote(argv[2])+L" "+argv[3];
    SECURITY_ATTRIBUTES inherit{sizeof(SECURITY_ATTRIBUTES),nullptr,TRUE};
    Handle nul(CreateFileW(L"NUL",GENERIC_READ|GENERIC_WRITE,FILE_SHARE_READ|FILE_SHARE_WRITE,&inherit,OPEN_EXISTING,0,nullptr));
    wchar_t raw[40]; swprintf_s(raw,L"%llx",reinterpret_cast<unsigned long long>(nul.value)); SetEnvironmentVariableW(L"LATCH_NULL_HANDLE",raw);
    auto ntopen=reinterpret_cast<decltype(&NtOpenFile)>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"),"NtOpenFile"));
    wchar_t kname[]=LR"(\Device\KsecDD)"; UNICODE_STRING kn{static_cast<USHORT>(wcslen(kname)*2),sizeof(kname),kname};
    OBJECT_ATTRIBUTES ka{sizeof(ka),nullptr,&kn,OBJ_CASE_INSENSITIVE|OBJ_INHERIT,nullptr,nullptr}; IO_STATUS_BLOCK ks{};Handle kh;
    NTSTATUS kstatus=ntopen(&kh.value,0x100003,&ka,&ks,FILE_SHARE_READ|FILE_SHARE_WRITE,0);
    if(kstatus<0) fail(L"ksec",static_cast<DWORD>(kstatus));
    swprintf_s(raw,L"%llx",reinterpret_cast<unsigned long long>(kh.value)); SetEnvironmentVariableW(L"LATCH_KSEC_HANDLE",raw);
    Handle job(CreateJobObjectW(nullptr,nullptr));
    if(!job.value)fail(L"create job");
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
    limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if(!SetInformationJobObject(job.value,JobObjectExtendedLimitInformation,&limits,sizeof(limits)))fail(L"job limits");
    // Assign the child atomically at creation, before any possible runner
    // teardown. No suspended child can be stranded between create and assign.
    if (!UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                                    &job.value, sizeof(job.value), nullptr, nullptr))
      fail(L"job attribute");
    std::array<Handle, 3> standard_handles;
    std::array<HANDLE, 3> inherited_handles{};
    HANDLE* standard_outputs[] = {&si.StartupInfo.hStdInput,
        &si.StartupInfo.hStdOutput, &si.StartupInfo.hStdError};
    for (size_t i = 0; i < standard_handles.size(); ++i) {
      HANDLE source = *standard_outputs[i];
      if (source == nullptr || source == INVALID_HANDLE_VALUE) source = nul.value;
      if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                            &standard_handles[i].value, 0, TRUE, DUPLICATE_SAME_ACCESS))
        fail(L"standard handle copy");
      *standard_outputs[i] = inherited_handles[i] = standard_handles[i].value;
    }
    if (!UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        inherited_handles.data(), sizeof(inherited_handles), nullptr, nullptr))
      fail(L"handle allowlist");
    PROCESS_INFORMATION pi{};
    if (!CreateProcessAsUserW(restricted.value,argv[2],line.data(),nullptr,nullptr,TRUE,EXTENDED_STARTUPINFO_PRESENT|CREATE_NO_WINDOW|CREATE_SUSPENDED,nullptr,argv[1],&si.StartupInfo,&pi)) fail(L"CreateProcessW");
    Handle process(pi.hProcess); Handle thread(pi.hThread);
    LatchHandles devices{};
    if (!DuplicateHandle(GetCurrentProcess(), nul.value, pi.hProcess,
                          &devices.null_device, 0, FALSE, DUPLICATE_SAME_ACCESS) ||
        !DuplicateHandle(GetCurrentProcess(), kh.value, pi.hProcess,
                          &devices.crypto_device, 0, FALSE, DUPLICATE_SAME_ACCESS) ||
        !DetourCopyPayloadToProcess(pi.hProcess, latch_handles_id, &devices, sizeof(devices)))
      fail(L"device payload");
    wchar_t object_path[1024]{};
    ULONG object_length=0;
    if(!GetAppContainerNamedObjectPath(nullptr,sid,1024,object_path,&object_length)) fail(L"AC object path");
    using OpenDirectory=NTSTATUS(NTAPI*)(PHANDLE,ACCESS_MASK,POBJECT_ATTRIBUTES);
    auto open_directory=reinterpret_cast<OpenDirectory>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"),"NtOpenDirectoryObject"));
    DWORD session=0;if(!ProcessIdToSessionId(GetCurrentProcessId(),&session))fail(L"session");
    std::wstring absolute_objects=L"\\Sessions\\"+std::to_wstring(session)+L"\\"+object_path;
    UNICODE_STRING object_name{static_cast<USHORT>(absolute_objects.size()*2),static_cast<USHORT>(absolute_objects.size()*2),absolute_objects.data()};
    OBJECT_ATTRIBUTES directory_attributes{sizeof(directory_attributes),nullptr,&object_name,OBJ_CASE_INSENSITIVE,nullptr,nullptr};
    Handle object_directory;
    NTSTATUS object_status=open_directory(&object_directory.value,READ_CONTROL|WRITE_DAC|0xf,&directory_attributes);
    if(object_status<0)fail(L"open AC namespace",static_cast<DWORD>(object_status));
    PACL directory_acl=nullptr;PSECURITY_DESCRIPTOR directory_sd=nullptr;
    DWORD directory_code=GetSecurityInfo(object_directory.value,SE_KERNEL_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&directory_acl,nullptr,&directory_sd);
    if(directory_code)fail(L"get AC namespace ACL",directory_code);
    EXPLICIT_ACCESSW directory_entry{};directory_entry.grfAccessPermissions=0xf|READ_CONTROL;directory_entry.grfAccessMode=GRANT_ACCESS;
    directory_entry.Trustee.TrusteeForm=TRUSTEE_IS_SID;directory_entry.Trustee.ptstrName=static_cast<LPWSTR>(write_sid.value);
    PACL directory_updated=nullptr;
    directory_code=SetEntriesInAclW(1,&directory_entry,directory_acl,&directory_updated);
    if(!directory_code)directory_code=SetSecurityInfo(object_directory.value,SE_KERNEL_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,directory_updated,nullptr);
    LocalFree(directory_updated);LocalFree(directory_sd);
    if(directory_code)fail(L"grant AC namespace",directory_code);
    wchar_t module[32768]; const DWORD module_length=GetModuleFileNameW(nullptr,module,32768);
    if(!module_length || module_length>=32768) fail(L"module path");
    std::wstring hook(module,module_length); hook.resize(hook.find_last_of(L"\\/")+1); hook+=L"latch-boundary-compat.dll";
    const int count=WideCharToMultiByte(CP_UTF8,0,hook.c_str(),-1,nullptr,0,nullptr,nullptr);
    std::string hook_utf8(static_cast<size_t>(count),0);
    WideCharToMultiByte(CP_UTF8,0,hook.c_str(),-1,hook_utf8.data(),count,nullptr,nullptr);
    LPCSTR dll=hook_utf8.c_str();
    if(!DetourUpdateProcessWithDll(pi.hProcess,&dll,1)) {TerminateProcess(pi.hProcess,125);fail(L"inject");}
    ResumeThread(pi.hThread);
    const DWORD waited = WaitForSingleObject(process.value,timeout_ms);
    if (waited == WAIT_FAILED) fail(L"wait child");
    DWORD code=0; GetExitCodeProcess(process.value,&code);
    if(waited == WAIT_TIMEOUT) { TerminateJobObject(job.value,124); code = 124; }
    std::fwprintf(stderr,L"child exit %lx\n",code); result=static_cast<int>(code);
    TerminateJobObject(job.value,125);
    WaitForSingleObject(process.value,5000);
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting{};
    for (;;) {
      if (!QueryInformationJobObject(job.value, JobObjectBasicAccountingInformation,
                                     &accounting, sizeof(accounting), nullptr)) fail(L"query job exit");
      if (accounting.ActiveProcesses == 0) break;
      Sleep(1);
    }
    DeleteProcThreadAttributeList(attrs);
  } catch(const Error& e) { std::fwprintf(stderr,L"%ls: %lu\n",e.api,e.code); }
    catch(const std::exception& e) { std::fprintf(stderr, "Windows boundary: %s", e.what()); }
  FreeSid(sid);
  DeleteAppContainerProfile(name.c_str());
  return result;
}
