#include "common.h"

#include <objbase.h>
#include <shlobj.h>
#include <userenv.h>

#include <limits>
#include <utility>

#include "appcontainer.h"
#include "job.h"
#include "recovery.h"
#include "recovery_store.h"
#include "token.h"

namespace latch {
using namespace recovery_store;
namespace {
void reject_reparse_tree(const std::filesystem::path& path) {
  PinnedObject pinned(path, FILE_READ_ATTRIBUTES | READ_CONTROL);
  if (std::filesystem::is_directory(path)) {
    for (const auto& child : std::filesystem::directory_iterator(path))
      reject_reparse_tree(child.path());
  }
}
}  // namespace

void Recovery::recover_pending() {
  const DWORD attributes = GetFileAttributesW(pending_.c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    if (GetLastError() == ERROR_FILE_NOT_FOUND) return;
    fail(L"inspect pending recovery");
  }
  PinnedObject pending_pin(pending_, FILE_READ_ATTRIBUTES | READ_CONTROL);
  const auto completion = pending_ / L"complete";
  if (GetFileAttributesW(completion.c_str()) != INVALID_FILE_ATTRIBUTES) {
    require(read_record(completion) == std::vector<std::wstring>{L"complete"},
            L"invalid completion marker");
    for (const auto& file : std::filesystem::directory_iterator(pending_)) {
      PinnedObject pin(file.path(), FILE_READ_ATTRIBUTES | READ_CONTROL);
      const auto name = file.path().filename().wstring();
      require(name == L"complete" || name.ends_with(L".rec") ||
                  name.ends_with(L".tmp"),
              L"unexpected completed journal content");
    }
    for (const auto& file : std::filesystem::directory_iterator(pending_))
      if (file.path() != completion && !DeleteFileW(file.path().c_str()))
        fail(L"finish intent deletion");
    if (!DeleteFileW(completion.c_str())) fail(L"finish completion deletion");
    CloseHandle(pending_pin.object.value);
    pending_pin.object.value = nullptr;
    if (!RemoveDirectoryW(pending_.c_str())) fail(L"finish journal deletion");
    return;
  }
  std::vector<std::filesystem::path> records;
  for (const auto& file : std::filesystem::directory_iterator(pending_)) {
    const auto name = file.path().filename().wstring();
    if (name == L"reservation.stage") {
      PinnedObject pin(file.path(), FILE_READ_ATTRIBUTES | READ_CONTROL);
      continue;
    }
    const bool record_file = name.size() == 12 && name.ends_with(L".rec");
    if (name == L"complete.tmp") continue;
    const bool temporary = name.size() == 16 && name.ends_with(L".rec.tmp");
    require(
        (record_file || temporary) &&
            std::all_of(name.begin(), name.begin() + 8,
                        [](wchar_t ch) { return ch >= L'0' && ch <= L'9'; }),
        L"unexpected recovery journal content");
    PinnedObject pin(file.path(), FILE_READ_ATTRIBUTES | READ_CONTROL);
    if (record_file) records.push_back(file.path());
  }
  std::sort(records.begin(), records.end());
  if (records.empty()) {
    // Unpublished header implies no sandbox mutation could have begun.
    for (const auto& file : std::filesystem::directory_iterator(pending_))
      if (!DeleteFileW(file.path().c_str()))
        fail(L"remove incomplete journal header");
    pending_pin.object.value = (CloseHandle(pending_pin.object.value), nullptr);
    if (!RemoveDirectoryW(pending_.c_str()))
      fail(L"remove empty recovery transaction");
    return;
  }
  for (size_t index = 0; index < records.size(); ++index) {
    wchar_t expected[32]{};
    swprintf_s(expected, L"%08u.rec", static_cast<unsigned>(index));
    require(records[index].filename() == expected,
            L"recovery sequence is incomplete; preserve the journal");
  }
  auto header = read_record(records.front());
  require(header.size() == 8 && header[0] == L"header" && header[1] == L"1" &&
              header[2] == current_user() &&
              header[3].starts_with(L"LatchProbe.S-1-5-21-") &&
              header[6].starts_with(L"Global\\LatchBoundary-S-1-5-21-"),
          L"invalid recovery header identity");
  profile_ = header[3];
  package_sid_ = header[4];
  write_sid_ = header[5];
  job_ = header[6];
  PSID derived = nullptr;
  if (FAILED(DeriveAppContainerSidFromAppContainerName(profile_.c_str(),
                                                       &derived)))
    fail(L"derive stale package");
  auto expected_sid = parse_sid(package_sid_.c_str());
  const bool same_sid = EqualSid(derived, expected_sid.value) != FALSE;
  FreeSid(derived);
  require(same_sid, L"recovery package SID mismatch");
  const auto expected_package = local_appdata() / L"Packages" / profile_;
  require(_wcsicmp(expected_package.c_str(), header[7].c_str()) == 0,
          L"recovery package path mismatch");
  drain_stale_job(job_);
  struct AclRollback {
    ObjectState original;
    std::set<std::wstring> versions;
  };
  std::map<std::wstring, AclRollback> objects;
  std::vector<std::pair<std::wstring, std::wstring>> roots, reservations;
  std::map<std::wstring, std::wstring> reservation_intents;
  std::wstring package_identity;
  bool rollback_complete = false;
  for (const auto& file : records) {
    const auto row = read_record(file);
    if (row == std::vector<std::wstring>{L"rollback-complete"})
      rollback_complete = true;
  }
  // Unpublished intents cannot have authorized a mutation. Discard torn
  // temporary writes before reusing the next immutable sequence number.
  for (const auto& file : std::filesystem::directory_iterator(pending_))
    if (file.path().extension() == L".tmp" && !DeleteFileW(file.path().c_str()))
      fail(L"remove unpublished intent");
  sequence_ = 0;
  for (const auto& file : records) {
    const unsigned index =
        static_cast<unsigned>(std::stoul(file.filename().wstring()));
    sequence_ = std::max(sequence_, index + 1);
    if (file == records.front()) continue;
    const auto row = read_record(file);
    require(!row.empty(), L"empty recovery record");
    if (row[0] == L"acl") {
      require(row.size() == 5, L"invalid ACL recovery record");
      auto [it, inserted] = objects.try_emplace(
          row[2], AclRollback{{row[1], row[2], row[3]}, {}});
      (void)inserted;
      it->second.versions.insert(row[3]);
      it->second.versions.insert(row[4]);
      const std::wstring package_prefix =
          expected_package.wstring() +
          std::filesystem::path::preferred_separator;
      const bool owned_package =
          _wcsicmp(row[1].c_str(), expected_package.c_str()) == 0 ||
          _wcsnicmp(row[1].c_str(), package_prefix.c_str(),
                    package_prefix.size()) == 0;
      if (rollback_complete && owned_package) {
        objects.erase(row[2]);
        continue;
      }
      // The primary path is pinned in the complete preflight below. A
      // second hardlink name must still designate the recorded identity.
      if (!inserted && it->second.original.path != row[1]) {
        PinnedObject alias(row[1]);
        require(alias.state().identity == row[2],
                L"recovery hardlink alias was replaced");
      }
    } else if (row[0] == L"root") {
      require(row.size() == 3, L"invalid grant-root record");
      roots.emplace_back(row[1], row[2]);
    } else if (row[0] == L"reservation-intent") {
      require(row.size() == 3, L"invalid reservation intent");
      reservation_intents[row[1]] = row[2];
    } else if (row[0] == L"reservation") {
      require(row.size() == 3, L"invalid reservation record");
      reservations.emplace_back(row[1], row[2]);
    } else if (row[0] == L"package-intent") {
      require(row.size() == 3 &&
                  _wcsicmp(row[1].c_str(), expected_package.c_str()) == 0,
              L"invalid package creation intent");
      PinnedObject parent(expected_package.parent_path());
      require(parent.state().identity == row[2],
              L"package parent was replaced");
    } else if (row[0] == L"package") {
      require(row.size() == 3 &&
                  _wcsicmp(row[1].c_str(), expected_package.c_str()) == 0,
              L"invalid package identity record");
      package_identity = row[2];
    } else if (row[0] == L"rollback-complete") {
      require(row.size() == 1, L"invalid rollback seal");
    } else
      fail(L"unknown recovery record", ERROR_INVALID_DATA);
  }
  // Preflight every existing object before restoring any ACL. A host edit
  // not described by this transaction is a conflict, never an overwrite.
  std::vector<const AclRollback*> ordered;
  for (const auto& [id, rollback] : objects) {
    (void)id;
    ordered.push_back(&rollback);
  }
  std::sort(ordered.begin(), ordered.end(), [](const auto* a, const auto* b) {
    return a->original.path.size() < b->original.path.size();
  });
  std::vector<PinnedObject> pins;
  for (const auto* entry : ordered) {
    const auto& rollback = *entry;
    pins.emplace_back(rollback.original.path);
    const auto now = pins.back().state();
    if (now.identity != rollback.original.identity ||
        !rollback.versions.contains(now.security)) {
      std::fwprintf(
          stderr,
          L"Recovery ACL conflict: %ls. Original/expected descriptors are in "
          L"%ls; reconcile the host edit before retrying.\n",
          rollback.original.path.c_str(), pending_.c_str());
      fail(L"recovery refuses unrelated ACL changes", ERROR_REVISION_MISMATCH);
    }
  }
  for (const auto& [path, id] : roots) {
    PinnedObject pin(path);
    require(pin.state().identity == id, L"recovery grant root was replaced");
  }
  for (const auto& [path, parent] : reservation_intents) {
    PinnedObject pin(std::filesystem::path(path).parent_path());
    require(pin.state().identity == parent, L"reservation parent was replaced");
    if (GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES) {
      const auto match = std::find_if(
          reservations.begin(), reservations.end(), [&](const auto& item) {
            return _wcsicmp(item.first.c_str(), path.c_str()) == 0;
          });
      require(match != reservations.end(),
              L"unsealed .git reservation exists; inspect it before recovery");
      PinnedObject marker(path);
      require(marker.state().identity == match->second,
              L".git reservation replaced; refusing removal");
    } else
      require(GetLastError() == ERROR_FILE_NOT_FOUND,
              L"cannot inspect .git reservation");
  }
  bool package_exists =
      GetFileAttributesW(expected_package.c_str()) != INVALID_FILE_ATTRIBUTES;
  if (package_exists) {
    if (package_identity.empty()) {
      std::fwprintf(stderr,
                    L"Unsealed AppContainer creation: %ls. Preserve journal "
                    L"%ls; an operator must verify the package and SID mapping "
                    L"before removal. Automatic execution is blocked.%lc",
                    expected_package.c_str(), pending_.c_str(), 10);
      fail(L"profile creation interrupted before identity seal",
           ERROR_RECOVERY_FAILURE);
    }
    PinnedObject package(expected_package);
    require(package.state().identity == package_identity,
            L"AppContainer directory replaced; refusing cleanup");
    reject_reparse_tree(expected_package);
  } else
    require(GetLastError() == ERROR_FILE_NOT_FOUND ||
                GetLastError() == ERROR_PATH_NOT_FOUND,
            L"cannot inspect stale package");
  // New developer-created objects have no pre-call descriptor. Remove only
  // this transaction's unique ACEs, preserving every unrelated ACE/control.
  // Write these removals ahead too, so recovery itself is restartable.
  auto package_sid = parse_sid(package_sid_.c_str());
  auto restrictor = parse_sid(write_sid_.c_str());
  std::set<std::wstring> scanned;
  const auto visit = [&](const auto& self,
                         const std::filesystem::path& path) -> void {
    const auto attrs = GetFileAttributesW(path.c_str());
    if (attrs == INVALID_FILE_ATTRIBUTES) fail(L"scan recovery grant tree");
    if (attrs & FILE_ATTRIBUTE_REPARSE_POINT)
      return;  // never follow a link out of a grant tree
    PinnedObject pinned(path);
    const auto state = pinned.state();
    if (!scanned.insert(state.identity).second) return;
    if (!objects.contains(state.identity)) {
      auto sd = descriptor(state.security);
      PACL acl = nullptr;
      BOOL present = FALSE, defaulted = FALSE;
      if (!GetSecurityDescriptorDacl(sd.value, &present, &acl, &defaulted) ||
          !acl)
        fail(L"new object has unsupported ACL", ERROR_INVALID_ACL);
      std::vector<BYTE> storage(acl->AclSize);
      auto* clean = reinterpret_cast<PACL>(storage.data());
      if (!InitializeAcl(clean, acl->AclSize, acl->AclRevision))
        fail(L"new object cleanup ACL");
      for (DWORD i = 0; i < acl->AceCount; ++i) {
        void* ace = nullptr;
        if (!GetAce(acl, i, &ace)) fail(L"read new object ACE");
        const auto* header_ace = static_cast<ACE_HEADER*>(ace);
        if (header_ace->AceType == ACCESS_ALLOWED_ACE_TYPE ||
            header_ace->AceType == ACCESS_DENIED_ACE_TYPE) {
          auto* entry = static_cast<ACCESS_ALLOWED_ACE*>(ace);
          if (EqualSid(&entry->SidStart, package_sid.value) ||
              EqualSid(&entry->SidStart, restrictor.value))
            continue;
        }
        if (!AddAce(clean, acl->AclRevision, MAXDWORD, ace,
                    header_ace->AceSize))
          fail(L"preserve new object ACE");
      }
      const auto after = changed_dacl(state.security, clean, false);
      if (after != state.security) {
        // Original means the post-command descriptor minus our ACEs. For a
        // new file this is the only rollback that preserves its host edits.
        record({L"acl", state.path, state.identity, after, state.security});
        write_security(pinned.object.value, after);
      }
    }
    if (attrs & FILE_ATTRIBUTE_DIRECTORY)
      for (const auto& child : std::filesystem::directory_iterator(path))
        self(self, child.path());
  };

  size_t index = 0;
  for (const auto* entry : ordered) {
    const auto& rollback = *entry;
    auto& pinned = pins[index++];
    const auto current = read_security(pinned.object.value);
    require(rollback.versions.contains(current),
            L"host ACL changed during recovery");
    if (current != rollback.original.security)
      write_security(pinned.object.value, rollback.original.security);
    if (index == 1) pause(L"cleanup");
  }
  pins.clear();
  for (const auto& [path, id] : roots) {
    (void)id;
    visit(visit, path);
  }
  if (!rollback_complete) record({L"rollback-complete"});
  pause(L"rollback-sealed");
  for (const auto& [path, id] : reservations) {
    if (GetFileAttributesW(path.c_str()) == INVALID_FILE_ATTRIBUTES) {
      require(GetLastError() == ERROR_FILE_NOT_FOUND,
              L"cannot inspect reservation cleanup");
      continue;
    }
    PinnedObject pinned(path, READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE);
    require(pinned.state().identity == id,
            L"reservation changed during cleanup");
    FILE_DISPOSITION_INFO remove{TRUE};
    if (!SetFileInformationByHandle(pinned.object.value, FileDispositionInfo,
                                    &remove, sizeof(remove)))
      fail(L"remove stale .git reservation");
  }
  validate_profile_mapping(profile_, package_sid_, false);
  const auto removed = DeleteAppContainerProfile(profile_.c_str());
  if (FAILED(removed) && removed != HRESULT_FROM_WIN32(ERROR_NOT_FOUND) &&
      removed != HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND))
    fail(L"delete stale AppContainer profile", static_cast<DWORD>(removed));
  if (GetFileAttributesW(expected_package.c_str()) != INVALID_FILE_ATTRIBUTES &&
      std::filesystem::is_empty(expected_package)) {
    PinnedObject pinned(expected_package,
                        READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE);
    require(pinned.state().identity == package_identity,
            L"staged package identity changed");
    FILE_DISPOSITION_INFO remove{TRUE};
    if (!SetFileInformationByHandle(pinned.object.value, FileDispositionInfo,
                                    &remove, sizeof(remove)))
      fail(L"remove unregistered package");
  }
  pause(L"profile-removed");
  require(
      GetFileAttributesW(expected_package.c_str()) == INVALID_FILE_ATTRIBUTES &&
          (GetLastError() == ERROR_FILE_NOT_FOUND ||
           GetLastError() == ERROR_PATH_NOT_FOUND),
      L"AppContainer package remains after cleanup");
  // The completion marker is durable before deleting any intent. A crash
  // while unlinking journal files can then finish deletion without rollback.
  durable_record(pending_ / L"complete", {L"complete"});
  for (const auto& file : std::filesystem::directory_iterator(pending_))
    if (file.path().filename() != L"complete" &&
        !DeleteFileW(file.path().c_str()))
      fail(L"remove recovered intent");
  if (!DeleteFileW((pending_ / L"complete").c_str()))
    fail(L"remove recovery completion");
  CloseHandle(pending_pin.object.value);
  pending_pin.object.value = nullptr;
  if (!RemoveDirectoryW(pending_.c_str()))
    fail(L"remove completed recovery journal");
  sequence_ = 0;
}
}  // namespace latch
