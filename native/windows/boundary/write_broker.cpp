#include "write_broker.h"
#include "broker_path.h"

namespace latch {
namespace {
auto nt_create() {
  return reinterpret_cast<decltype(&NtCreateFile)>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateFile"));
}
void require_status(NTSTATUS status, const wchar_t* operation) {
  if (status >= 0) return;
  using Convert = ULONG(NTAPI*)(NTSTATUS);
  const auto convert = reinterpret_cast<Convert>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "RtlNtStatusToDosError"));
  fail(operation, convert(status));
}
std::wstring final_path(HANDLE file) {
  wchar_t buffer[32768]{};
  const DWORD length = GetFinalPathNameByHandleW(file, buffer, 32768, FILE_NAME_NORMALIZED);
  if (!length || length >= 32768) fail(L"write broker final path");
  std::wstring result(buffer, length);
  if (result.starts_with(L"\\\\?\\")) result.erase(0, 4);
  return broker_normalize(result);
}
bool same_object(HANDLE first, HANDLE second) {
  FILE_ID_INFO a{}, b{};
  if (!GetFileInformationByHandleEx(first, FileIdInfo, &a, sizeof(a)) ||
      !GetFileInformationByHandleEx(second, FileIdInfo, &b, sizeof(b)))
    fail(L"write broker scope identity");
  return a.VolumeSerialNumber == b.VolumeSerialNumber &&
      memcmp(&a.FileId, &b.FileId, sizeof(a.FileId)) == 0;
}
}

WriteBroker::WriteBroker(const std::vector<std::wstring>& roots,
                         const std::vector<std::wstring>& denied,
                         const std::vector<std::wstring>& denied_write,
                         const std::vector<std::wstring>& protected_git) {
  for (const auto& root : roots) {
    roots_.push_back(broker_normalize(root));
    pins_.emplace_back(root, FILE_READ_ATTRIBUTES);
  }
  for (const auto& path : denied) denied_.push_back(broker_normalize(path));
  for (const auto& path : denied_write) denied_.push_back(broker_normalize(path));
  for (const auto& root : protected_git) git_.push_back(broker_normalize(root));
  // A protected subtree's ancestors cannot be renamed to evade path masks.
  for (const auto& path : denied_) {
    if (GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES)
      pins_.emplace_back(path, FILE_READ_ATTRIBUTES);
  }
}

bool WriteBroker::allowed(const std::wstring& path) const {
  for (const auto& denied : denied_)
    if (broker_path_within(path, denied)) return false;
  for (const auto& root : git_) {
    if (!broker_path_within(path, root)) continue;
    for (const auto& component : std::filesystem::path(path).lexically_relative(root))
      if (_wcsicmp(component.c_str(), L".git") == 0) return false;
  }
  return std::any_of(roots_.begin(), roots_.end(), [&](const auto& root) { return broker_path_within(path, root); });
}

void WriteBroker::check_parent(const PinnedObject& parent, const std::wstring& path) const {
  const auto actual_parent = final_path(parent.object.value);
  if (_wcsicmp(actual_parent.c_str(), std::filesystem::path(path).parent_path().c_str()) != 0)
    fail(L"write broker parent path changed", ERROR_ACCESS_DENIED);
  bool scoped = false;
  for (size_t index = 0; index < roots_.size(); ++index) {
    if (!broker_path_within(path, roots_[index])) continue;
    if (same_object(parent.object.value, pins_[index].object.value)) scoped = true;
    for (const auto& ancestor : parent.ancestors)
      if (same_object(ancestor.value, pins_[index].object.value)) scoped = true;
  }
  if (!scoped) fail(L"write broker parent root identity mismatch", ERROR_ACCESS_DENIED);
  std::vector<HANDLE> ancestry{parent.object.value};
  for (const auto& ancestor : parent.ancestors) ancestry.push_back(ancestor.value);
  for (const auto object : ancestry) {
    FILE_CASE_SENSITIVE_INFO sensitivity{};
    if (GetFileInformationByHandleEx(object, FileCaseSensitiveInfo, &sensitivity, sizeof(sensitivity)) &&
        sensitivity.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR)
      fail(L"write broker refuses case-sensitive parent", ERROR_NOT_SUPPORTED);
  }
}

void WriteBroker::check(HANDLE file, const PinnedObject& parent, const std::wstring& path) const {
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(file, FileAttributeTagInfo, &tag, sizeof(tag))) fail(L"write broker object attributes");
  if (tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) fail(L"write broker refuses reparses", ERROR_ACCESS_DENIED);
  const auto actual = final_path(file);
  if (!allowed(actual) || _wcsicmp(actual.c_str(), path.c_str()) != 0)
    fail(L"write broker final scope mismatch", ERROR_ACCESS_DENIED);
  bool identity_scope = false;
  for (size_t index = 0; index < roots_.size(); ++index) {
    if (!broker_path_within(actual, roots_[index])) continue;
    if (same_object(file, pins_[index].object.value) || same_object(parent.object.value, pins_[index].object.value))
      identity_scope = true;
    for (const auto& ancestor : parent.ancestors)
      if (same_object(ancestor.value, pins_[index].object.value)) identity_scope = true;
  }
  if (!identity_scope) fail(L"write broker root identity mismatch", ERROR_ACCESS_DENIED);
  std::vector<HANDLE> ancestry{parent.object.value, file};
  for (const auto& ancestor : parent.ancestors) ancestry.push_back(ancestor.value);
  for (const auto object : ancestry) {
    FILE_CASE_SENSITIVE_INFO sensitivity{};
    if (GetFileInformationByHandleEx(object, FileCaseSensitiveInfo, &sensitivity, sizeof(sensitivity)) &&
        sensitivity.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR)
      fail(L"write broker refuses case-sensitive paths", ERROR_NOT_SUPPORTED);
  }
  BY_HANDLE_FILE_INFORMATION info{};
  if (!GetFileInformationByHandle(file, &info)) fail(L"write broker hardlink count");
  if (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY || info.nNumberOfLinks <= 1) return;
  wchar_t volume[32768]{}, alias[32768]{};
  if (!GetVolumePathNameW(actual.c_str(), volume, 32768)) fail(L"write broker volume");
  DWORD size = 32768;
  HANDLE iterator = FindFirstFileNameW(actual.c_str(), 0, &size, alias);
  if (iterator == INVALID_HANDLE_VALUE) fail(L"write broker hardlinks");
  struct Close { HANDLE handle; ~Close() { FindClose(handle); } } close{iterator};
  do {
    const auto target = (std::filesystem::path(volume) / std::filesystem::path(alias).relative_path()).wstring();
    if (!allowed(broker_normalize(target))) fail(L"write broker refuses outside hardlink", ERROR_ACCESS_DENIED);
    size = 32768;
  } while (FindNextFileNameW(iterator, &size, alias));
  if (GetLastError() != ERROR_HANDLE_EOF) fail(L"write broker remaining links");
}

void WriteBroker::open(const LatchReadRequest& request, LatchReadResponse& response, HANDLE process) {
  constexpr ACCESS_MASK rights = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
  GENERIC_MAPPING mapping{FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_GENERIC_EXECUTE, FILE_ALL_ACCESS};
  ACCESS_MASK access = request.access;
  MapGenericMask(&access, &mapping);
  if ((access & ~rights) || !request.path_length || request.path_length >= 32768 ||
      request.disposition == FILE_SUPERSEDE || request.disposition > FILE_OVERWRITE_IF ||
      request.allocation_size != 0 || request.options & FILE_OPEN_BY_FILE_ID)
    fail(L"write broker refuses unsupported open", ERROR_ACCESS_DENIED);
  const auto path = broker_normalize(std::wstring(request.path, request.path_length));
  if (!allowed(path)) fail(L"write broker path denied", ERROR_ACCESS_DENIED);
  const auto target = std::filesystem::path(path);
  PinnedObject parent(target.parent_path(), FILE_READ_ATTRIBUTES);
  if (std::none_of(roots_.begin(), roots_.end(), [&](const auto& root) { return _wcsicmp(root.c_str(), path.c_str()) == 0; }))
    check_parent(parent, path);
  else if (request.disposition != FILE_OPEN)
    fail(L"write broker cannot replace a scope root", ERROR_ACCESS_DENIED);
  auto name = target.filename().wstring();
  UNICODE_STRING text{static_cast<USHORT>(name.size() * 2), static_cast<USHORT>(name.size() * 2), name.data()};
  OBJECT_ATTRIBUTES attributes{sizeof(attributes), parent.object.value, &text,
                               OBJ_CASE_INSENSITIVE | 0x1000 /* OBJ_DONT_REPARSE */, nullptr, nullptr};
  IO_STATUS_BLOCK io{};
  Handle file;
  const ULONG options = (request.options & ~FILE_DELETE_ON_CLOSE) | FILE_OPEN_REPARSE_POINT;
  NTSTATUS status = nt_create()(&file.value, access | FILE_READ_ATTRIBUTES, &attributes, &io,
      nullptr, request.file_attributes, request.share & ~FILE_SHARE_DELETE, FILE_OPEN, options, nullptr, 0);
  bool created = false;
  if (status == static_cast<NTSTATUS>(0xc0000034u) &&
      (request.disposition == FILE_CREATE || request.disposition == FILE_OPEN_IF || request.disposition == FILE_OVERWRITE_IF)) {
    status = nt_create()(&file.value, access | FILE_READ_ATTRIBUTES, &attributes, &io,
        nullptr, request.file_attributes, request.share & ~FILE_SHARE_DELETE, FILE_CREATE, options, nullptr, 0);
    created = status >= 0;
  } else if (status >= 0 && request.disposition == FILE_CREATE) {
    fail(L"write broker existing create target", ERROR_FILE_EXISTS);
  }
  require_status(status, L"write broker open");
  check(file.value, parent, path);
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(file.value, FileAttributeTagInfo, &tag, sizeof(tag)))
    fail(L"write broker opened type");
  if ((tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) &&
      (access & (FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_DELETE_CHILD)))
    fail(L"directory namespace writes require mediated operations", ERROR_ACCESS_DENIED);
  if (!created && (request.disposition == FILE_OVERWRITE || request.disposition == FILE_OVERWRITE_IF)) {
    if (!(access & FILE_WRITE_DATA)) fail(L"write broker truncate requires write data", ERROR_ACCESS_DENIED);
    FILE_END_OF_FILE_INFO length{};
    if (!SetFileInformationByHandle(file.value, FileEndOfFileInfo, &length, sizeof(length)))
      fail(L"write broker truncate");
  }
  if (request.options & FILE_DELETE_ON_CLOSE) {
    FILE_DISPOSITION_INFO remove{TRUE};
    if (!SetFileInformationByHandle(file.value, FileDispositionInfo, &remove, sizeof(remove)))
      fail(L"write broker delete on close");
  }
  if (!DuplicateHandle(GetCurrentProcess(), file.value, process, &response.file, access, FALSE, 0))
    fail(L"write broker duplicate capability");
  response.information = created ? FILE_CREATED :
      (request.disposition == FILE_OVERWRITE || request.disposition == FILE_OVERWRITE_IF ? FILE_OVERWRITTEN : FILE_OPENED);
  response.error = ERROR_SUCCESS;
}

void WriteBroker::rename(const LatchReadRequest& request, LatchReadResponse& response, HANDLE process) {
  if (!request.source_handle || !request.path_length || request.path_length >= 32768 ||
      request.rename_flags & ~3u)
    fail(L"write broker refuses rename flags", ERROR_ACCESS_DENIED);
  const auto target = broker_normalize(std::wstring(request.path, request.path_length));
  if (!allowed(target)) fail(L"write broker rename target denied", ERROR_ACCESS_DENIED);
  Handle source;
  if (!DuplicateHandle(process, request.source_handle, GetCurrentProcess(), &source.value,
                       0, FALSE, DUPLICATE_SAME_ACCESS)) fail(L"write broker source capability");
  if (GetFileType(source.value) != FILE_TYPE_DISK) fail(L"write broker source is not a file", ERROR_ACCESS_DENIED);
  const auto path = final_path(source.value);
  if (!allowed(path)) fail(L"write broker rename source denied", ERROR_ACCESS_DENIED);
  PinnedObject source_parent(std::filesystem::path(path).parent_path(), FILE_READ_ATTRIBUTES);
  check(source.value, source_parent, path);
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(source.value, FileAttributeTagInfo, &tag, sizeof(tag)))
    fail(L"write broker rename source type");
  // POSIX replacement may bypass no-delete sharing. Never let it replace a
  // directory containing pinned policy roots or protected descendants.
  if ((request.rename_flags & 2u) && (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY))
    fail(L"write broker refuses POSIX directory replacement", ERROR_ACCESS_DENIED);
  using Query = NTSTATUS(NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
  const auto query = reinterpret_cast<Query>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtQueryInformationFile"));
  ACCESS_MASK access = 0;
  IO_STATUS_BLOCK io{};
  require_status(query(source.value, &io, &access, sizeof(access),
                       static_cast<FILE_INFORMATION_CLASS>(8)), L"write broker source rights");
  if (request.operation == LatchBrokerOperation::rename && !(access & DELETE))
    fail(L"write broker rename requires delete capability", ERROR_ACCESS_DENIED);
  const auto destination = std::filesystem::path(target);
  PinnedObject parent(destination.parent_path(), FILE_READ_ATTRIBUTES);
  check_parent(parent, target);
  // Root-relative native operations never parse a mutable ancestor pathname.
  const auto name = destination.filename().wstring();
  const size_t size = offsetof(FILE_RENAME_INFO, FileName) + name.size() * sizeof(wchar_t);
  std::vector<BYTE> buffer(size);
  auto* information = reinterpret_cast<FILE_RENAME_INFO*>(buffer.data());
  information->Flags = request.rename_flags;
  information->RootDirectory = parent.object.value;
  information->FileNameLength = static_cast<DWORD>(name.size() * sizeof(wchar_t));
  memcpy(information->FileName, name.data(), name.size() * sizeof(wchar_t));
  using Set = NTSTATUS(NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
  const auto set = reinterpret_cast<Set>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetInformationFile"));
  const auto kind = static_cast<FILE_INFORMATION_CLASS>(request.operation == LatchBrokerOperation::rename ? 65 : 72);
  require_status(set(source.value, &io, information, static_cast<ULONG>(size), kind), L"write broker rename/link");
  response.error = ERROR_SUCCESS;
  response.information = io.Information;
}
}  // namespace latch
