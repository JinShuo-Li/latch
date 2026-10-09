#include "read_broker.h"
#include "broker_protocol.h"
#include "recovery_store.h"
#include "token.h"

namespace latch {
namespace {
bool within(const std::wstring& path, const std::wstring& root) {
  return _wcsicmp(path.c_str(), root.c_str()) == 0 ||
      (path.size() > root.size() &&
       _wcsnicmp(path.c_str(), root.c_str(), root.size()) == 0 &&
       path[root.size()] == L'\\');
}
std::wstring normalize(const std::wstring& input) {
  require(input.size() >= 3 && input[1] == L':' && input[2] == L'\\',
          L"broker requires a local absolute path");
  // Reject streams, device syntax, embedded NULs and Win32 ambiguous names.
  require(input.find(L':', 2) == std::wstring::npos &&
              input.find(L'\0') == std::wstring::npos &&
              input.find(L'/') == std::wstring::npos,
          L"unsupported broker path syntax");
  auto path = std::filesystem::path(input).lexically_normal();
  for (const auto& component : path.relative_path()) {
    const auto text = component.wstring();
    if (text.empty()) continue;  // A trailing directory separator is valid.
    require(text.back() != L'.' && text.back() != L' ' &&
                text.find_first_of(L"*?\"") == std::wstring::npos,
            L"ambiguous broker path component");
  }
  auto value = path.wstring();
  while (value.size() > 3 && value.back() == L'\\') value.pop_back();
  return value;
}
}  // namespace

ReadBroker::ReadBroker(HANDLE job, PSID package,
                      const std::vector<std::wstring>& roots,
                      const std::vector<std::wstring>& denied)
    : job_(job), package_(GetLengthSid(package)) {
  if (!CopySid(static_cast<DWORD>(package_.size()), package_.data(), package))
    fail(L"copy broker package identity");
  for (const auto& root : roots) {
    roots_.push_back(normalize(root));
    roots_pinned_.emplace_back(root, FILE_READ_ATTRIBUTES);
  }
  for (const auto& path : denied) denied_.push_back(normalize(path));
  const auto name = L"\\\\.\\pipe\\LatchRead-" + unique_sid_string();
  auto sd = descriptor(recovery_store::user_acl());
  SECURITY_ATTRIBUTES sa{sizeof(sa), sd.value, FALSE};
  server_.value = CreateNamedPipeW(name.c_str(),
      PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
      PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT |
          PIPE_REJECT_REMOTE_CLIENTS,
      1, sizeof(LatchReadRequest) * 2, sizeof(LatchReadRequest) * 2, 0, &sa);
  if (server_.value == INVALID_HANDLE_VALUE) fail(L"create read broker pipe");
  client_.value = CreateFileW(name.c_str(), GENERIC_READ | GENERIC_WRITE,
      0, &sa, OPEN_EXISTING, 0, nullptr);
  if (client_.value == INVALID_HANDLE_VALUE) fail(L"connect read broker pipe");
  DWORD mode = PIPE_READMODE_MESSAGE;
  if (!SetNamedPipeHandleState(client_.value, &mode, nullptr, nullptr))
    fail(L"set read broker message mode");
  mutex_.value = CreateMutexW(&sa, FALSE, nullptr);
  if (!mutex_.value) fail(L"create read broker transaction mutex");
  thread_ = std::thread([this] { serve(); });
}

ReadBroker::~ReadBroker() {
  stopping_.store(true);
  if (thread_.joinable()) {
    CancelSynchronousIo(thread_.native_handle());
    DisconnectNamedPipe(server_.value);
    thread_.join();
  }
}

bool ReadBroker::allowed(const std::wstring& path) const {
  if (std::any_of(denied_.begin(), denied_.end(),
                 [&](const auto& root) { return within(path, root); }))
    return false;
  return std::any_of(roots_.begin(), roots_.end(),
                    [&](const auto& root) { return within(path, root); });
}

void ReadBroker::check_links(HANDLE file, const std::wstring& path) const {
  BY_HANDLE_FILE_INFORMATION info{};
  if (!GetFileInformationByHandle(file, &info)) fail(L"broker file links");
  if (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY || info.nNumberOfLinks <= 1)
    return;
  wchar_t volume[32768]{};
  if (!GetVolumePathNameW(path.c_str(), volume, 32768))
    fail(L"broker hardlink volume");
  wchar_t name[32768]{};
  DWORD size = 32768;
  HANDLE iterator = FindFirstFileNameW(path.c_str(), 0, &size, name);
  if (iterator == INVALID_HANDLE_VALUE) fail(L"broker hardlink enumeration");
  struct CloseFind { HANDLE handle; ~CloseFind() { FindClose(handle); } } close{iterator};
  do {
    const auto alias = (std::filesystem::path(volume) /
        std::filesystem::path(name).relative_path()).wstring();
    if (!allowed(normalize(alias))) fail(L"broker refuses outside hardlink", ERROR_ACCESS_DENIED);
    size = 32768;
  } while (FindNextFileNameW(iterator, &size, name));
  if (GetLastError() != ERROR_HANDLE_EOF) fail(L"broker remaining hardlinks");
}

void ReadBroker::serve() {
  while (!stopping_.load()) {
    LatchReadRequest request{};
    DWORD bytes = 0;
    if (!ReadFile(server_.value, &request, sizeof(request), &bytes, nullptr)) break;
    if (bytes < offsetof(LatchReadRequest, path) ||
        request.path_length >= 32768 ||
        bytes != latch_read_request_size(request.path_length) ||
        request.version != latch_broker_version) break;
    LatchReadResponse response{latch_broker_version, request.process_id,
                              request.request_id, ERROR_ACCESS_DENIED, nullptr};
    try {
      Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION |
                                 PROCESS_DUP_HANDLE, FALSE, request.process_id));
      if (!process.value) fail(L"broker caller process");
      BOOL in_job = FALSE;
      if (!IsProcessInJob(process.value, job_, &in_job) || !in_job)
        fail(L"broker caller is outside its job", ERROR_ACCESS_DENIED);
      Handle token;
      if (!OpenProcessToken(process.value, TOKEN_QUERY, &token.value))
        fail(L"broker caller token");
      auto info = token_info(token.value, TokenAppContainerSid);
      auto* app = reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(info.data());
      if (!app->TokenAppContainer || !EqualSid(app->TokenAppContainer, package_.data()))
        fail(L"broker caller package mismatch", ERROR_ACCESS_DENIED);
      constexpr ACCESS_MASK reads = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
      GENERIC_MAPPING mapping{FILE_GENERIC_READ, FILE_GENERIC_WRITE,
                              FILE_GENERIC_EXECUTE, FILE_ALL_ACCESS};
      ACCESS_MASK access = request.access;
      MapGenericMask(&access, &mapping);
      // No maximum-access, write, delete, ACL, owner, or security privilege.
      if ((access & ~reads) != 0 || request.path_length == 0 ||
          request.path_length >= 32768 ||
          (request.options & (FILE_DELETE_ON_CLOSE | FILE_OPEN_BY_FILE_ID)) != 0)
        fail(L"broker refuses non-read request", ERROR_ACCESS_DENIED);
      const auto path = normalize(std::wstring(request.path, request.path_length));
      if (!allowed(path)) fail(L"broker path denied", ERROR_ACCESS_DENIED);
      PinnedObject pin(path, FILE_READ_ATTRIBUTES);
      for (HANDLE object : [&] {
        std::vector<HANDLE> values;
        for (const auto& ancestor : pin.ancestors) values.push_back(ancestor.value);
        values.push_back(pin.object.value);
        return values;
      }()) {
        FILE_CASE_SENSITIVE_INFO sensitivity{};
        if (GetFileInformationByHandleEx(object, FileCaseSensitiveInfo,
              &sensitivity, sizeof(sensitivity)) &&
            (sensitivity.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR))
          fail(L"broker refuses case-sensitive directories", ERROR_NOT_SUPPORTED);
      }
      wchar_t final[32768]{};
      const DWORD final_length = GetFinalPathNameByHandleW(pin.object.value,
          final, 32768, FILE_NAME_NORMALIZED);
      if (!final_length || final_length >= 32768)
        fail(L"broker final object path");
      std::wstring final_path(final, final_length);
      if (final_path.starts_with(L"\\\\?\\")) final_path.erase(0, 4);
      if (!allowed(normalize(final_path)))
        fail(L"broker refuses aliased protected path", ERROR_ACCESS_DENIED);
      check_links(pin.object.value, path);
      // Pin every path component through the final open. The returned object
      // keeps no-delete sharing for the lifetime of the sandbox handle.
      const DWORD flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT |
          ((request.options & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT))
               ? 0 : FILE_FLAG_OVERLAPPED);
      Handle file(CreateFileW(path.c_str(), access | FILE_READ_ATTRIBUTES,
          request.share & ~FILE_SHARE_DELETE, nullptr, OPEN_EXISTING, flags, nullptr));
      if (file.value == INVALID_HANDLE_VALUE) fail(L"broker open read object");
      FILE_ATTRIBUTE_TAG_INFO tag{};
      if (!GetFileInformationByHandleEx(file.value, FileAttributeTagInfo, &tag, sizeof(tag)) ||
          tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
        fail(L"broker refuses reparse object", ERROR_ACCESS_DENIED);
      if (!DuplicateHandle(GetCurrentProcess(), file.value, process.value,
                           &response.file, access, FALSE, 0))
        fail(L"broker duplicate read capability");
      response.error = ERROR_SUCCESS;
    } catch (const Error& error) {
      response.error = error.code;
#ifdef LATCH_RECOVERY_TESTING
      if (GetEnvironmentVariableW(L"LATCH_BROKER_DIAGNOSTICS", nullptr, 0))
        std::fwprintf(stderr, L"Broker %ls: %lu access=%lx options=%lx path=%.*ls\n",
            error.api, error.code, request.access, request.options,
            static_cast<int>(std::min<DWORD>(request.path_length, 32767)), request.path);
#endif
    } catch (...) {
      response.error = ERROR_ACCESS_DENIED;
    }
    if (!WriteFile(server_.value, &response, sizeof(response), &bytes, nullptr) ||
        bytes != sizeof(response)) break;
  }
}
}  // namespace latch
