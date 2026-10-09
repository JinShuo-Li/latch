#include "filesystem_acl.h"

#include <limits>
namespace latch {
namespace {
struct FindNameHandle {
  HANDLE value = INVALID_HANDLE_VALUE;
  ~FindNameHandle() {
    if (value != INVALID_HANDLE_VALUE) FindClose(value);
  }
};

void capture_grant_identity(HANDLE object, GrantTarget& target) {
  FILE_BASIC_INFO basic{};
  if (!GetFileInformationByHandleEx(object, FileIdInfo, &target.file_id,
                                    sizeof(target.file_id)) ||
      !GetFileInformationByHandleEx(object, FileBasicInfo, &basic,
                                    sizeof(basic)))
    fail(L"capture grant identity");
  target.creation_time = basic.CreationTime.QuadPart;
}

void verify_grant_identity(HANDLE object, const GrantTarget& target) {
  FILE_ID_INFO file_id{};
  FILE_BASIC_INFO basic{};
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(object, FileIdInfo, &file_id,
                                    sizeof(file_id)) ||
      !GetFileInformationByHandleEx(object, FileBasicInfo, &basic,
                                    sizeof(basic)) ||
      !GetFileInformationByHandleEx(object, FileAttributeTagInfo, &tag,
                                    sizeof(tag)))
    fail(L"verify grant identity");
  require(file_id.VolumeSerialNumber == target.file_id.VolumeSerialNumber &&
              std::memcmp(file_id.FileId.Identifier,
                          target.file_id.FileId.Identifier,
                          sizeof(file_id.FileId.Identifier)) == 0 &&
              basic.CreationTime.QuadPart == target.creation_time &&
              ((tag.FileAttributes ^ target.attributes) &
               FILE_ATTRIBUTE_DIRECTORY) == 0 &&
              !(tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT),
          L"grant target was renamed, replaced, or changed type");
}

bool path_in_root(const std::wstring& path, const std::wstring& root) {
  if (_wcsicmp(path.c_str(), root.c_str()) == 0) return true;
  return path.size() > root.size() &&
         _wcsnicmp(path.c_str(), root.c_str(), root.size()) == 0 &&
         std::filesystem::path::preferred_separator == path[root.size()];
}

bool validate_hardlinks(const std::filesystem::path& path,
                        const std::vector<std::wstring>& allowed,
                        const std::wstring& current_root, DWORD link_count,
                        const Recovery& recovery) {
  if (link_count <= 1) return true;
  wchar_t volume[32768]{};
  if (!GetVolumePathNameW(path.c_str(), volume, 32768))
    fail(L"hardlink volume");
  std::vector<wchar_t> name(32768);
  DWORD length = static_cast<DWORD>(name.size());
  FindNameHandle iterator{
      FindFirstFileNameW(path.c_str(), 0, &length, name.data())};
  if (iterator.value == INVALID_HANDLE_VALUE) fail(L"enumerate hardlinks");
  bool safe = true;
  bool current_root_link = false;
  std::wstring representative;
  DWORD error = ERROR_SUCCESS;
  do {
    auto link = std::filesystem::path(volume) /
                std::filesystem::path(name.data()).relative_path();
    const auto canonical = std::filesystem::canonical(link).wstring();
    if (recovery.protected_journal_path(canonical)) {
      safe = false;
      break;
    }
    safe = std::any_of(allowed.begin(), allowed.end(),
                       [&](const std::wstring& root) {
                         return path_in_root(canonical, root);
                       });
    if (!safe) break;
    if (path_in_root(canonical, current_root) &&
        (!current_root_link || _wcsicmp(canonical.c_str(),
                                        representative.c_str()) < 0)) {
      representative = canonical;
      current_root_link = true;
    }
    length = static_cast<DWORD>(name.size());
  } while (FindNextFileNameW(iterator.value, &length, name.data()));
  if (safe) error = GetLastError();
  if (!safe)
    fail(L"write root contains an outside hardlink", ERROR_ACCESS_DENIED);
  if (error != ERROR_HANDLE_EOF)
    fail(L"enumerate remaining hardlinks", error);
  require(current_root_link, L"hardlink has no path in its grant root");
  const auto canonical_current = std::filesystem::canonical(path).wstring();
  return _wcsicmp(representative.c_str(), canonical_current.c_str()) == 0;
}
}  // namespace

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

DWORD update_acl_pair(PinnedObject& pinned, PSID first_sid, PSID second_sid,
                      DWORD rights, DWORD inheritance,
                      const Cancellation& cancel, Recovery& recovery) {
  // Shared roots need both identities. Apply them in one journaled mutation so
  // a crash can restore the exact original descriptor with one record.
  cancel.check();
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
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

bool prepare_acl_pair(PinnedObject& pinned, PSID first_sid, PSID second_sid,
                      DWORD rights, DWORD inheritance,
                      const Cancellation& cancel, AclChange& change) {
  cancel.check();
  PACL old_acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  const auto before = pinned.state();
  const DWORD code = GetSecurityInfo(
      pinned.object.value, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
      nullptr, nullptr, &old_acl, nullptr, &descriptor);
  Local descriptor_owner(descriptor);
  if (code != ERROR_SUCCESS) fail(L"read ACL for grant", code);
  // A NULL DACL already allows access. Replacing it with package-only ACEs
  // removes host rights and can make identity-based crash recovery impossible.
  if (!old_acl) return false;
  EXPLICIT_ACCESSW entries[2]{};
  PSID sids[2] = {first_sid, second_sid};
  const size_t entry_count = second_sid ? 2 : 1;
  for (size_t i = 0; i < entry_count; ++i) {
    entries[i].grfAccessPermissions = rights;
    entries[i].grfAccessMode = GRANT_ACCESS;
    entries[i].grfInheritance = inheritance;
    entries[i].Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entries[i].Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    entries[i].Trustee.ptstrName = static_cast<LPWSTR>(sids[i]);
  }
  PACL new_acl = nullptr;
  const DWORD acl_code = SetEntriesInAclW(static_cast<ULONG>(entry_count),
                                          entries, old_acl, &new_acl);
  Local new_acl_owner(new_acl);
  if (acl_code != ERROR_SUCCESS) fail(L"prepare grant ACL", acl_code);
  change.pinned = &pinned;
  change.before = before;
  change.after = changed_dacl(before.security, new_acl, false);
  return change.before.security != change.after;
}

DWORD update_acl_pair(const std::wstring& path, PSID first_sid,
                      PSID second_sid, DWORD rights, DWORD inheritance,
                      const Cancellation& cancel, Recovery& recovery) {
  PinnedObject pinned(path);
  return update_acl_pair(pinned, first_sid, second_sid, rights, inheritance,
                         cancel, recovery);
}

// ACL grants mutate the NTFS object shared by all hardlinks. Validate every
// link in every approved root before any grant is applied. Keep only compact
// identity records and a flat path arena, not one open handle per file.
void validate_grant_tree(const std::filesystem::path& path,
                         const std::vector<std::wstring>& allowed,
                         GrantPlan& plan, const Cancellation& cancel,
                         Recovery& recovery, bool recursive) {
  plan.root = path;
  plan.recursive = recursive;
  const auto visit = [&](const auto& self,
                         const std::filesystem::path& current) -> void {
    cancel.check();
    if (recovery.protected_journal_path(current)) return;
    Handle object(CreateFileW(
        current.c_str(), READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
    if (object.value == INVALID_HANDLE_VALUE) fail(L"open grant object");
    BY_HANDLE_FILE_INFORMATION information{};
    if (!GetFileInformationByHandle(object.value, &information))
      fail(L"grant object identity");
    const DWORD attributes = information.dwFileAttributes;
    if (attributes & FILE_ATTRIBUTE_REPARSE_POINT) return;
    if (attributes & FILE_ATTRIBUTE_DIRECTORY) {
      for (const auto& child : std::filesystem::directory_iterator(current)) {
        if (!recursive) {
          const auto extension = child.path().extension().wstring();
          if (_wcsicmp(extension.c_str(), L".exe") != 0 &&
              _wcsicmp(extension.c_str(), L".dll") != 0 &&
              _wcsicmp(extension.c_str(), L".pyd") != 0) continue;
          const auto child_attributes = GetFileAttributesW(child.path().c_str());
          if (child_attributes == INVALID_FILE_ATTRIBUTES) fail(L"bootstrap file attributes");
          if (child_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)) continue;
        }
        self(self, child.path());
      }
    } else if (!validate_hardlinks(current, allowed, plan.root.wstring(),
                                   information.nNumberOfLinks, recovery)) {
      return;
    }
    const auto relative_path = current.lexically_relative(plan.root);
    require(!relative_path.empty() ||
                _wcsicmp(current.c_str(), plan.root.c_str()) == 0,
            L"grant target escaped its root");
    require(!relative_path.is_absolute(), L"absolute grant-relative path");
    for (const auto& component : relative_path)
      require(component != L"..", L"grant target escaped its root");
    auto relative = relative_path.wstring();
    if (relative == L".") relative.clear();
    require(relative.size() <= std::numeric_limits<uint16_t>::max(),
            L"grant path exceeds supported length");
    require(plan.relative_paths.size() <=
                std::numeric_limits<uint32_t>::max() - relative.size(),
            L"grant path index exceeds supported size");
    GrantTarget target;
    target.relative_path_offset =
        static_cast<uint32_t>(plan.relative_paths.size());
    target.relative_path_length = static_cast<uint16_t>(relative.size());
    target.attributes = attributes;
    capture_grant_identity(object.value, target);
    plan.relative_paths.append(relative);
    plan.targets.push_back(target);
  };
  visit(visit, path);
}

void protect_sensitive_tree(const std::filesystem::path& input,
                            std::set<std::wstring>& visited,
                            const Cancellation& cancel, Recovery& recovery,
                            bool audit_only) {
  cancel.check();
  const auto path = std::filesystem::canonical(input).wstring();
  if (recovery.protected_journal_path(path))
    fail(L"sensitive path aliases protected journal", ERROR_ACCESS_DENIED);
  if (!visited.insert(path).second) return;
  PACL acl = nullptr;
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PinnedObject pinned(path, audit_only ? READ_CONTROL | FILE_READ_ATTRIBUTES :
                                        READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES);
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
  bool package_grant = false;
  for (DWORD i = 0; i < acl->AceCount; ++i) {
    void* ace = nullptr;
    if (!GetAce(acl, i, &ace)) fail(L"read sensitive ACE");
    auto* header = static_cast<ACE_HEADER*>(ace);
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) {
      const auto* allowed = static_cast<const ACCESS_ALLOWED_ACE*>(ace);
      PSID principal = const_cast<DWORD*>(&allowed->SidStart);
      if (std::memcmp(GetSidIdentifierAuthority(principal), &package_authority,
                      sizeof(package_authority)) == 0) {
        package_grant = true;
        continue;
      }
    } else if (header->AceType != ACCESS_DENIED_ACE_TYPE) {
      fail(L"unsupported sensitive ACL entry", ERROR_INVALID_ACL);
    }
    // Preserve existing host rights explicitly while sealing inheritance.
    header->AceFlags &= ~INHERITED_ACE;
    if (!AddAce(updated, acl->AclRevision, MAXDWORD, ace, header->AceSize))
      fail(L"copy sensitive ACE");
  }
  if (!audit_only) {
    recovery.change(pinned, before, updated, true);
  } else if (package_grant) {
    PinnedObject mutable_object(path);
    const auto current = mutable_object.state();
    require(current.identity == before.identity && current.security == before.security,
            L"sensitive object changed during audit");
    recovery.change(mutable_object, before, updated, true);
  }
  if (std::filesystem::is_directory(path)) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      protect_sensitive_tree(child.path(), visited, cancel, recovery, audit_only);
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
void Grants::add_plan(GrantPlan& plan,
                      const std::vector<std::wstring>& allowed,
                      PSID other_sid, DWORD rights) {
  constexpr size_t batch_size = 32;
  std::vector<PinnedObject> pins;
  std::vector<AclChange> changes;
  pins.reserve(batch_size);
  changes.reserve(batch_size);
  const auto apply_batch = [&] {
    if (!changes.empty()) recovery.change_batch(changes);
    changes.clear();
    pins.clear();
  };
  for (const auto& target : plan.targets) {
    require(target.relative_path_offset <= plan.relative_paths.size() &&
                target.relative_path_length <=
                    plan.relative_paths.size() - target.relative_path_offset,
            L"invalid grant path index");
    std::filesystem::path path = plan.root;
    if (target.relative_path_length) {
      const auto* first = plan.relative_paths.data() +
                          target.relative_path_offset;
      path /= std::wstring(first, target.relative_path_length);
    }
    pins.emplace_back(path);
    auto& pinned = pins.back();
    verify_grant_identity(pinned.object.value, target);
    BY_HANDLE_FILE_INFORMATION information{};
    if (!GetFileInformationByHandle(pinned.object.value, &information))
      fail(L"read current grant links");
    validate_hardlinks(path, allowed, plan.root.wstring(),
                       information.nNumberOfLinks, recovery);
    const DWORD inheritance = (plan.recursive && (target.attributes & FILE_ATTRIBUTE_DIRECTORY))
                                  ? SUB_CONTAINERS_AND_OBJECTS_INHERIT
                                  : NO_INHERITANCE;
    AclChange change;
    if (prepare_acl_pair(pinned, sid_, other_sid, rights, inheritance,
                         cancel, change))
      changes.push_back(std::move(change));
    if (pins.size() == batch_size) apply_batch();
  }
  apply_batch();
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
