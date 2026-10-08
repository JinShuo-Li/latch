#include "recovery.h"

#include <objbase.h>
#include <shlobj.h>
#include <userenv.h>

#include <limits>
#include <utility>

#include "appcontainer.h"
#include "job.h"
#include "recovery_store.h"
#include "token.h"

namespace latch {
using namespace recovery_store;
Recovery::Recovery(const Cancellation& cancel) : cancel_(cancel) {
  root_ = local_appdata() / L"LatchBoundaryRecovery-v1";
#ifdef LATCH_RECOVERY_TESTING
  wchar_t override_path[32768]{};
  const DWORD length =
      GetEnvironmentVariableW(L"LATCH_RECOVERY_ROOT", override_path, 32768);
  if (length && length < 32768) root_ = override_path;
#endif
  PinnedObject parent(root_.parent_path(), FILE_READ_ATTRIBUTES | READ_CONTROL);
  auto sd = descriptor(user_acl());
  SECURITY_ATTRIBUTES attributes{sizeof(attributes), sd.value, FALSE};
  if (!CreateDirectoryW(root_.c_str(), &attributes) &&
      GetLastError() != ERROR_ALREADY_EXISTS)
    fail(L"create protected recovery directory");
  PinnedObject root(root_, FILE_READ_ATTRIBUTES | READ_CONTROL);
  // User/system only; package principals must never get journal read/write.
  auto expected = descriptor(user_acl());
  auto actual = descriptor(read_security(root.object.value));
  PACL a = nullptr, b = nullptr;
  BOOL present = FALSE, defaulted = FALSE;
  GetSecurityDescriptorDacl(actual.value, &present, &a, &defaulted);
  GetSecurityDescriptorDacl(expected.value, &present, &b, &defaulted);
  require(
      a && b && a->AclSize == b->AclSize && std::memcmp(a, b, a->AclSize) == 0,
      L"recovery directory ACL changed; restore user/system-only protection");
  SECURITY_DESCRIPTOR_CONTROL control{};
  DWORD revision = 0;
  PSID owner = nullptr;
  GetSecurityDescriptorControl(actual.value, &control, &revision);
  GetSecurityDescriptorOwner(actual.value, &owner, &defaulted);
  auto user = parse_sid(current_user().c_str());
  require((control & SE_DACL_PROTECTED) && owner && EqualSid(owner, user.value),
          L"recovery directory owner or inheritance changed");
  root_pins_ = std::move(root.ancestors);
  root_pins_.push_back(std::move(root.object));
  const auto lock = root_ / L"owner.lock";
  for (;;) {
    lock_.value = CreateFileW(
        lock.c_str(), GENERIC_READ | GENERIC_WRITE, 0, &attributes, OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    if (lock_.value != INVALID_HANDLE_VALUE) break;
    if (GetLastError() != ERROR_SHARING_VIOLATION)
      fail(L"lock recovery directory");
    cancel_.check();
    Sleep(20);
  }
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(lock_.value, FileAttributeTagInfo, &tag,
                                    sizeof(tag)) ||
      (tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT))
    fail(L"invalid recovery lock", ERROR_INVALID_DATA);
  pending_ = root_ / L"pending";
  {
    TimingScope timer(boundary_timing().rollback);
    recover_pending();
  }
}
void Recovery::record(const std::vector<std::wstring>& fields) {
  wchar_t name[32]{};
  swprintf_s(name, L"%08u.rec", sequence_);
  {
    TimingScope timer(boundary_timing().journal);
    durable_record(pending_ / name, fields);
  }
  ++sequence_;
  if (boundary_timing().enabled) ++boundary_timing().journal_records;
}
void Recovery::record_batch(
    const std::vector<std::vector<std::wstring>>& rows) {
  constexpr size_t max_rows = 32;
  constexpr size_t max_bytes = 8 * 1024 * 1024;
  size_t begin = 0;
  while (begin < rows.size()) {
    size_t end = begin;
    size_t bytes = 0;
    while (end < rows.size() && end - begin < max_rows) {
      const auto& row = rows[end];
      require(row.size() == 5 && row[0] == L"acl",
              L"invalid ACL batch entry");
      size_t row_bytes = 5 * sizeof(uint32_t);
      for (const auto& field : row) {
        require(field.size() <= 1024 * 1024,
                L"recovery field too large");
        row_bytes += field.size() * sizeof(wchar_t);
      }
      if (end != begin && bytes + row_bytes > max_bytes) break;
      require(row_bytes <= max_bytes, L"recovery ACL batch too large");
      bytes += row_bytes;
      ++end;
    }
    std::vector<std::wstring> fields{L"acl-batch-v1",
                                     std::to_wstring(end - begin)};
    for (size_t i = begin; i < end; ++i)
      fields.insert(fields.end(), rows[i].begin(), rows[i].end());
    wchar_t name[32]{};
    swprintf_s(name, L"%08u.rec", sequence_);
    {
      TimingScope timer(boundary_timing().journal);
      durable_record(pending_ / name, fields);
    }
    ++sequence_;
    if (boundary_timing().enabled)
      boundary_timing().journal_records += end - begin;
    begin = end;
  }
}
void Recovery::begin() {
  profile_ = L"LatchProbe." + unique_sid_string();
  write_sid_ = unique_sid_string();
  job_ = L"Global\\LatchBoundary-" + unique_sid_string();
  PSID sid = nullptr;
  const auto code =
      DeriveAppContainerSidFromAppContainerName(profile_.c_str(), &sid);
  if (FAILED(code))
    fail(L"derive journal profile SID", static_cast<DWORD>(code));
  LPWSTR text = nullptr;
  if (!ConvertSidToStringSidW(sid, &text)) {
    FreeSid(sid);
    fail(L"journal profile SID");
  }
  package_sid_ = text;
  Local text_owner(text);
  FreeSid(sid);
  validate_profile_mapping(profile_, package_sid_, true);
  const auto package = local_appdata() / L"Packages" / profile_;
  require(GetFileAttributesW(package.c_str()) == INVALID_FILE_ATTRIBUTES &&
              GetLastError() == ERROR_FILE_NOT_FOUND,
          L"new AppContainer path already exists");
  auto security = descriptor(user_acl());
  SECURITY_ATTRIBUTES attributes{sizeof(attributes), security.value, FALSE};
  if (!CreateDirectoryW(pending_.c_str(), &attributes))
    fail(L"create recovery transaction");
  record({L"header", L"1", current_user(), profile_, package_sid_, write_sid_,
          job_, package.wstring()});
  pause(L"after-journal");
}
void Recovery::pause(const wchar_t* point) const {
#ifdef LATCH_RECOVERY_TESTING
  wchar_t selected[100]{};
  if (!GetEnvironmentVariableW(L"LATCH_RECOVERY_PAUSE", selected, 100) ||
      std::wcscmp(selected, point))
    return;
  // Test build only: a host fixture kills this PID with TerminateProcess.
  const auto marker = root_ / L"pause.pid.tmp";
  Handle file(CreateFileW(marker.c_str(), GENERIC_WRITE, FILE_SHARE_READ,
                          nullptr, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
                          nullptr));
  if (file.value == INVALID_HANDLE_VALUE) fail(L"write crash-test PID");
  const DWORD pid = GetCurrentProcessId();
  DWORD written = 0;
  if (!WriteFile(file.value, &pid, sizeof(pid), &written, nullptr) ||
      written != sizeof(pid) || !FlushFileBuffers(file.value))
    fail(L"flush crash-test PID");
  CloseHandle(file.value);
  file.value = nullptr;
  if (!MoveFileExW(marker.c_str(), (root_ / L"pause.pid").c_str(),
                   MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH))
    fail(L"publish crash-test PID");
  const auto release = root_ / L"pause.resume";
  while (GetFileAttributesW(release.c_str()) == INVALID_FILE_ATTRIBUTES)
    Sleep(20);
#else
  (void)point;
#endif
}
void Recovery::change(PinnedObject& pinned, const ObjectState& before, PACL acl,
                      bool protect) {
  cancel_.check();
  const auto current = pinned.state();
  require(current.identity == before.identity &&
              current.security == before.security,
          L"host object changed while computing sandbox ACL");
  const auto after = changed_dacl(before.security, acl, protect);
  if (before.security == after) return;
  record({L"acl", before.path, before.identity, before.security, after});
  {
    TimingScope timer(boundary_timing().acl_apply);
    write_security(pinned.object.value, after);
  }
  if (boundary_timing().enabled) ++boundary_timing().acl_mutations;
  if (++mutations_ == 1) pause(L"first-acl");
}
void Recovery::change_batch(const std::vector<AclChange>& changes) {
  std::vector<std::vector<std::wstring>> rows;
  rows.reserve(changes.size());
  for (const auto& change : changes) {
    require(change.pinned != nullptr, L"missing pinned ACL object");
    cancel_.check();
    const auto current = change.pinned->state();
    require(current.identity == change.before.identity &&
                current.security == change.before.security,
            L"host object changed while preparing sandbox ACL batch");
    if (change.before.security != change.after)
      rows.push_back({L"acl", change.before.path, change.before.identity,
                      change.before.security, change.after});
  }
  if (rows.empty()) return;
  record_batch(rows);
  for (const auto& change : changes) {
    if (change.before.security == change.after) continue;
    cancel_.check();
    const auto current = change.pinned->state();
    require(current.identity == change.before.identity &&
                current.security == change.before.security,
            L"host object changed before sandbox ACL batch apply");
    {
      TimingScope timer(boundary_timing().acl_apply);
      write_security(change.pinned->object.value, change.after);
    }
    if (boundary_timing().enabled) ++boundary_timing().acl_mutations;
    if (++mutations_ == 1) pause(L"first-acl");
  }
}
void Recovery::track_root(const std::filesystem::path& path) {
  PinnedObject pinned(path);
  const auto state = pinned.state();
  // A workspace may contain the protected journal (for example the user's
  // home). Grant traversal and rollback both exclude that subtree.
  if (protected_journal_path(state.path))
    fail(L"grant includes protected journal", ERROR_ACCESS_DENIED);
  record({L"root", state.path, state.identity});
}
bool Recovery::protected_journal_path(const std::filesystem::path& path) const {
  const auto value = std::filesystem::absolute(path).lexically_normal().wstring();
  const auto root = root_.lexically_normal().wstring();
  return value.size() >= root.size() &&
         _wcsnicmp(value.c_str(), root.c_str(), root.size()) == 0 &&
         (value.size() == root.size() || value[root.size()] == L'\\');
}
Handle Recovery::reserve_git(const std::filesystem::path& path) {
  PinnedObject parent(path.parent_path());
  const auto staging = pending_ / L"reservation.stage";
  record({L"reservation-intent", path.wstring(), parent.state().identity});
  Handle file(CreateFileW(
      staging.c_str(), GENERIC_READ | DELETE, FILE_SHARE_READ, nullptr,
      CREATE_NEW, FILE_ATTRIBUTE_HIDDEN | FILE_FLAG_DELETE_ON_CLOSE, nullptr));
  if (file.value == INVALID_HANDLE_VALUE) fail(L"stage .git reservation");
  record({L"reservation", path.wstring(), identity(file.value)});
  const auto name = std::filesystem::absolute(path).wstring();
  std::vector<BYTE> storage(sizeof(FILE_RENAME_INFO) +
                            name.size() * sizeof(wchar_t));
  auto* rename = reinterpret_cast<FILE_RENAME_INFO*>(storage.data());
  rename->ReplaceIfExists = FALSE;
  rename->RootDirectory = nullptr;
  rename->FileNameLength = static_cast<DWORD>(name.size() * sizeof(wchar_t));
  std::memcpy(rename->FileName, name.data(), rename->FileNameLength);
  // Same-volume atomic rename: the durable identity precedes its public name.
  // Cross-volume reservations fail closed instead of copying an untracked file.
  if (!SetFileInformationByHandle(file.value, FileRenameInfo, rename,
                                  static_cast<DWORD>(storage.size())))
    fail(
        L"publish .git reservation (journal and workspace must share a "
        L"volume)");
  return file;
}
void Recovery::prepare_profile() {
  const auto target = local_appdata() / L"Packages" / profile_;
  PinnedObject parent(target.parent_path());
  // Recheck both namespaces immediately before the durable creation intent.
  // The unique name and derived SID identify the profile even if the API
  // replaces its package directory before returning.
  validate_profile_mapping(profile_, package_sid_, true);
  require(GetFileAttributesW(target.c_str()) == INVALID_FILE_ATTRIBUTES &&
              GetLastError() == ERROR_FILE_NOT_FOUND,
          L"AppContainer package appeared before creation intent");
  record({L"package-intent", target.wstring(), parent.state().identity});
  pause(L"profile-intent");
}
std::filesystem::path Recovery::scratch_path() const {
  return local_appdata() / L"Packages" / profile_ / L"AC" / L"Temp";
}
void Recovery::profile_created() {
  const auto path = local_appdata() / L"Packages" / profile_;
  PinnedObject pinned(path);
  const auto state = pinned.state();
  record({L"package", state.path, state.identity});
  pause(L"appcontainer");
}

void Recovery::finish() { recover_pending(); }
void Recovery::execution_start() { record({L"execution-start"}); }

}  // namespace latch
