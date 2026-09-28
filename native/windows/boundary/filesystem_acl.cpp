#include "filesystem_acl.h"
namespace latch {
DWORD update_acl(const std::wstring& path, PSID sid, ACCESS_MODE mode,
                 DWORD rights, DWORD inheritance, const Cancellation& cancel,
                 Recovery& recovery) {
  cancel.check();
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PinnedObject pinned(path);
  const auto before = pinned.state();
  DWORD code = GetSecurityInfo(pinned.object.value, SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr,
                               &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) return code;
  if (mode == DENY_ACCESS) {
    // A protected DACL can contain an explicit allow before a converted
    // inherited deny. SetEntriesInAcl may merge at that old position. Our
    // deny must precede all allows, without changing their relative order.
    const DWORD bytes = (old_acl ? old_acl->AclSize : sizeof(ACL)) +
                        sizeof(ACCESS_DENIED_ACE) - sizeof(DWORD) +
                        GetLengthSid(sid);
    if (bytes > MAXWORD) return ERROR_ALLOTTED_SPACE_EXCEEDED;
    std::vector<BYTE> storage(bytes);
    auto* updated = reinterpret_cast<PACL>(storage.data());
    if (!InitializeAcl(updated, bytes, ACL_REVISION_DS) ||
        !AddAccessDeniedAceEx(updated, ACL_REVISION_DS, inheritance, rights,
                              sid))
      return GetLastError();
    if (old_acl) {
      for (DWORD i = 0; i < old_acl->AceCount; ++i) {
        void* ace = nullptr;
        if (!GetAce(old_acl, i, &ace)) return GetLastError();
        const auto* header = static_cast<const ACE_HEADER*>(ace);
        const auto* denied = static_cast<const ACCESS_DENIED_ACE*>(ace);
        if (header->AceType == ACCESS_DENIED_ACE_TYPE &&
            header->AceFlags == inheritance && denied->Mask == rights &&
            EqualSid(const_cast<DWORD*>(&denied->SidStart), sid))
          continue;
        if (!AddAce(updated, ACL_REVISION_DS, MAXDWORD, ace, header->AceSize))
          return GetLastError();
      }
    }
    recovery.change(pinned, before, updated);
    return ERROR_SUCCESS;
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
  recovery.change(pinned, before, new_acl);
  return ERROR_SUCCESS;
}

DWORD update_acl_pair(const std::wstring& path, PSID first_sid,
                      PSID second_sid, DWORD rights, DWORD inheritance,
                      const Cancellation& cancel, Recovery& recovery) {
  // Shared roots need both identities. Apply them in one journaled mutation so
  // a crash can restore the exact original descriptor with one record.
  cancel.check();
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PinnedObject pinned(path);
  const auto before = pinned.state();
  DWORD code = GetSecurityInfo(pinned.object.value, SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr,
                               &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) return code;
  EXPLICIT_ACCESSW entries[2]{};
  PSID sids[2] = {first_sid, second_sid};
  for (size_t i = 0; i < 2; ++i) {
    entries[i].grfAccessPermissions = rights;
    entries[i].grfAccessMode = GRANT_ACCESS;
    entries[i].grfInheritance = inheritance;
    entries[i].Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entries[i].Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    entries[i].Trustee.ptstrName = static_cast<LPWSTR>(sids[i]);
  }
  PACL new_acl = nullptr;
  code = SetEntriesInAclW(2, entries, old_acl, &new_acl);
  Local new_acl_owner(new_acl);
  if (code != ERROR_SUCCESS) return code;
  recovery.change(pinned, before, new_acl);
  return ERROR_SUCCESS;
}

// A write grant changes an NTFS object's ACL, which is shared by all hardlinks.
// Validate every link before granting a root; a name outside the approved roots
// must never obtain a write grant through an alias in the workspace.
void validate_write_tree(const std::filesystem::path& path,
                         const std::vector<std::wstring>& allowed,
                         std::vector<Handle>& locks, const Cancellation& cancel,
                         Recovery& recovery) {
  cancel.check();
  if (recovery.protected_journal_path(path)) return;
  Handle object(CreateFileW(
      path.c_str(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE,
      nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (object.value == INVALID_HANDLE_VALUE) fail(L"open write-grant object");
  BY_HANDLE_FILE_INFORMATION information{};
  if (!GetFileInformationByHandle(object.value, &information))
    fail(L"write-grant identity");
  locks.push_back(std::move(object));
  if (information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) return;
  if (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      validate_write_tree(child.path(), allowed, locks, cancel, recovery);
    return;
  }
  if (information.nNumberOfLinks <= 1) return;
  wchar_t volume[32768]{};
  if (!GetVolumePathNameW(path.c_str(), volume, 32768))
    fail(L"hardlink volume");
  std::vector<wchar_t> name(32768);
  DWORD length = static_cast<DWORD>(name.size());
  HANDLE iterator = FindFirstFileNameW(path.c_str(), 0, &length, name.data());
  if (iterator == INVALID_HANDLE_VALUE) fail(L"enumerate hardlinks");
  bool safe = true;
  DWORD error = ERROR_SUCCESS;
  do {
    auto link = std::filesystem::path(volume) /
                std::filesystem::path(name.data()).relative_path();
    const auto canonical = std::filesystem::canonical(link).wstring();
    safe = std::any_of(
        allowed.begin(), allowed.end(), [&](const std::wstring& root) {
          return canonical.size() > root.size() &&
                 _wcsnicmp(canonical.c_str(), root.c_str(), root.size()) == 0 &&
                 std::filesystem::path::preferred_separator ==
                     canonical[root.size()];
        });
    if (!safe) break;
    length = static_cast<DWORD>(name.size());
  } while (FindNextFileNameW(iterator, &length, name.data()));
  if (safe) error = GetLastError();
  FindClose(iterator);
  if (!safe)
    fail(L"write root contains an outside hardlink", ERROR_ACCESS_DENIED);
  if (error != ERROR_HANDLE_EOF) fail(L"enumerate remaining hardlinks", error);
}

void protect_sensitive_tree(const std::filesystem::path& input,
                            std::set<std::wstring>& visited,
                            const Cancellation& cancel, Recovery& recovery) {
  cancel.check();
  const auto path = std::filesystem::canonical(input).wstring();
  if (recovery.protected_journal_path(path))
    fail(L"sensitive path aliases protected journal", ERROR_ACCESS_DENIED);
  if (!visited.insert(path).second) return;
  PACL acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PinnedObject pinned(path);
  const auto before = pinned.state();
  DWORD code = GetSecurityInfo(pinned.object.value, SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr,
                               &acl, nullptr, &descriptor);
  Local owner(descriptor);
  if (code) fail(L"read sensitive ACL", code);
  if (!acl) fail(L"sensitive path has a NULL DACL", ERROR_INVALID_ACL);
  std::vector<BYTE> storage(acl->AclSize);
  auto* updated = reinterpret_cast<PACL>(storage.data());
  if (!InitializeAcl(updated, acl->AclSize, acl->AclRevision))
    fail(L"initialize sensitive ACL");
  const SID_IDENTIFIER_AUTHORITY package_authority =
      SECURITY_APP_PACKAGE_AUTHORITY;
  for (DWORD i = 0; i < acl->AceCount; ++i) {
    void* ace = nullptr;
    if (!GetAce(acl, i, &ace)) fail(L"read sensitive ACE");
    auto* header = static_cast<ACE_HEADER*>(ace);
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) {
      const auto* allowed = static_cast<const ACCESS_ALLOWED_ACE*>(ace);
      PSID principal = const_cast<DWORD*>(&allowed->SidStart);
      if (std::memcmp(GetSidIdentifierAuthority(principal), &package_authority,
                      sizeof(package_authority)) == 0)
        continue;
    } else if (header->AceType != ACCESS_DENIED_ACE_TYPE) {
      fail(L"unsupported sensitive ACL entry", ERROR_INVALID_ACL);
    }
    // Preserve existing host rights explicitly while sealing inheritance.
    header->AceFlags &= ~INHERITED_ACE;
    if (!AddAce(updated, acl->AclRevision, MAXDWORD, ace, header->AceSize))
      fail(L"copy sensitive ACE");
  }
  recovery.change(pinned, before, updated, true);
  if (std::filesystem::is_directory(path)) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      protect_sensitive_tree(child.path(), visited, cancel, recovery);
  }
}

void Grants::add(const std::wstring& path, ACCESS_MODE mode, DWORD rights) {
  if (recovery.protected_journal_path(path)) return;
  if (mode == DENY_ACCESS) {
    // Inheritance alone never protects a child with SE_DACL_PROTECTED.
    // Apply the deny to each existing object, including protected children.
    const auto resolved = std::filesystem::canonical(path).wstring();
    if (recovery.protected_journal_path(resolved))
      fail(L"deny path aliases protected journal", ERROR_ACCESS_DENIED);
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
  // Existing children are journaled explicitly; setting the parent never
  // implicitly changes an unrecorded descendant ACL.
  const auto attributes = GetFileAttributesW(path.c_str());
  if (attributes & FILE_ATTRIBUTE_REPARSE_POINT) return;
  if (attributes & FILE_ATTRIBUTE_DIRECTORY) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      add(child.path().wstring(), mode, rights);
  }
  add_one(path, mode, rights);
}
void Grants::add_pair(const std::wstring& path, PSID other_sid, DWORD rights) {
  if (recovery.protected_journal_path(path)) return;
  const DWORD attributes = GetFileAttributesW(path.c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) fail(L"paired grant attributes");
  if (attributes & FILE_ATTRIBUTE_REPARSE_POINT) return;
  if (attributes & FILE_ATTRIBUTE_DIRECTORY) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      add_pair(child.path().wstring(), other_sid, rights);
  }
  const DWORD inheritance = (attributes & FILE_ATTRIBUTE_DIRECTORY)
                                ? SUB_CONTAINERS_AND_OBJECTS_INHERIT
                                : NO_INHERITANCE;
  const DWORD code = update_acl_pair(path, sid_, other_sid, rights, inheritance,
                                     cancel, recovery);
  if (code != ERROR_SUCCESS) fail(L"paired grant ACL", code);
}
void Grants::add_one(const std::wstring& path, ACCESS_MODE mode, DWORD rights) {
  const DWORD attributes = GetFileAttributesW(path.c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) fail(L"grant attributes");
  const DWORD inheritance = (attributes & FILE_ATTRIBUTE_DIRECTORY)
                                ? SUB_CONTAINERS_AND_OBJECTS_INHERIT
                                : NO_INHERITANCE;
  const DWORD code =
      update_acl(path, sid_, mode, rights, inheritance, cancel, recovery);
  if (code != ERROR_SUCCESS) fail(L"SetNamedSecurityInfoW(grant)", code);
}
void GitReservation::create(const std::filesystem::path& workspace,
                            Recovery& recovery) {
  const auto marker = workspace / L".git";
  if (GetFileAttributesW(marker.c_str()) != INVALID_FILE_ATTRIBUTES) return;
  if (GetLastError() != ERROR_FILE_NOT_FOUND) fail(L"inspect .git reservation");
  Handle reservation = recovery.reserve_git(marker);
  handle.value = reservation.value;
  reservation.value = nullptr;
}
}  // namespace latch
