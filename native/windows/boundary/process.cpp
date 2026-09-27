#include "process.h"

#include "appcontainer.h"
#include "desktop.h"
#include "detours.h"
#include "handles.h"
#include "job.h"
#include "token.h"
namespace latch {
int execute_target(wchar_t** argv, const Cancellation& cancel,
                   HANDLE restricted, PSID write_sid, PSID sid,
                   LPPROC_THREAD_ATTRIBUTE_LIST attrs, DWORD timeout_ms,
                   Recovery& recovery) {
  PrivateDesktop desktop;
  desktop.create(write_sid, unique_sid_string());
  desktop.grant_package(sid);
  STARTUPINFOEXW si{};
  si.StartupInfo.cb = sizeof(si);
  si.lpAttributeList = attrs;
  si.StartupInfo.lpDesktop = desktop.name.data();
  si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
  si.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
  si.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
  si.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
  std::wstring line = quote(argv[2]) + L" " + argv[3];
  SECURITY_ATTRIBUTES inherit{sizeof(SECURITY_ATTRIBUTES), nullptr, TRUE};
  Handle nul(CreateFileW(L"NUL", GENERIC_READ | GENERIC_WRITE,
                         FILE_SHARE_READ | FILE_SHARE_WRITE, &inherit,
                         OPEN_EXISTING, 0, nullptr));
  wchar_t raw[40];
  swprintf_s(raw, L"%llx", reinterpret_cast<unsigned long long>(nul.value));
  SetEnvironmentVariableW(L"LATCH_NULL_HANDLE", raw);
  auto ntopen = reinterpret_cast<decltype(&NtOpenFile)>(
      GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtOpenFile"));
  wchar_t kname[] = LR"(\Device\KsecDD)";
  UNICODE_STRING kn{static_cast<USHORT>(wcslen(kname) * 2), sizeof(kname),
                    kname};
  OBJECT_ATTRIBUTES ka{sizeof(ka), nullptr,
                       &kn,        OBJ_CASE_INSENSITIVE | OBJ_INHERIT,
                       nullptr,    nullptr};
  IO_STATUS_BLOCK ks{};
  Handle kh;
  NTSTATUS kstatus = ntopen(&kh.value, 0x100003, &ka, &ks,
                            FILE_SHARE_READ | FILE_SHARE_WRITE, 0);
  if (kstatus < 0) fail(L"ksec", static_cast<DWORD>(kstatus));
  swprintf_s(raw, L"%llx", reinterpret_cast<unsigned long long>(kh.value));
  SetEnvironmentVariableW(L"LATCH_KSEC_HANDLE", raw);
  Job job(recovery.job_name());
  // Assign the child atomically at creation, before any possible runner
  // teardown. No suspended child can be stranded between create and assign.
  if (!UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                                 &job.handle.value, sizeof(job.handle.value),
                                 nullptr, nullptr))
    fail(L"job attribute");
  std::array<Handle, 3> standard_handles;
  std::array<HANDLE, 3> inherited_handles{};
  HANDLE* standard_outputs[] = {&si.StartupInfo.hStdInput,
                                &si.StartupInfo.hStdOutput,
                                &si.StartupInfo.hStdError};
  for (size_t i = 0; i < standard_handles.size(); ++i) {
    HANDLE source = *standard_outputs[i];
    if (source == nullptr || source == INVALID_HANDLE_VALUE) source = nul.value;
    if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                         &standard_handles[i].value, 0, TRUE,
                         DUPLICATE_SAME_ACCESS))
      fail(L"standard handle copy");
    *standard_outputs[i] = inherited_handles[i] = standard_handles[i].value;
  }
  if (!UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                 inherited_handles.data(),
                                 sizeof(inherited_handles), nullptr, nullptr))
    fail(L"handle allowlist");
  PROCESS_INFORMATION pi{};
  cancel.check();
  if (!CreateProcessAsUserW(
          restricted, argv[2], line.data(), nullptr, nullptr, TRUE,
          EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
          nullptr, argv[1], &si.StartupInfo, &pi))
    fail(L"CreateProcessW");
  Handle process(pi.hProcess);
  Handle thread(pi.hThread);
  recovery.pause(L"child-launch");
  LatchHandles devices{};
  Handle workspace(CreateFileW(argv[1], FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (workspace.value == INVALID_HANDLE_VALUE) fail(L"open workspace volume identity");
  FILE_ID_INFO workspace_id{};
  if (!GetFileInformationByHandleEx(workspace.value, FileIdInfo,
                                    &workspace_id, sizeof(workspace_id)))
    fail(L"workspace volume identity");
  // Host-opened ancestor handles carry metadata rights only.  They make
  // realpath/lstat traversal possible without changing a parent ACL.
  std::vector<Handle> ancestors;
  for (auto parent = std::filesystem::path(argv[1]).parent_path();
       !parent.empty(); parent = parent.parent_path()) {
    if (ancestors.size() == latch_max_ancestors)
      fail(L"workspace ancestry exceeds compatibility handle limit", ERROR_BUFFER_OVERFLOW);
    Handle ancestor(CreateFileW(parent.c_str(), FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
    if (ancestor.value == INVALID_HANDLE_VALUE) fail(L"open metadata ancestor");
    FILE_ATTRIBUTE_TAG_INFO tag{};
    if (!GetFileInformationByHandleEx(ancestor.value, FileAttributeTagInfo,
                                     &tag, sizeof(tag)) ||
        !(tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) ||
        (tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT))
      fail(L"invalid workspace metadata ancestor", ERROR_INVALID_DATA);
    ancestors.push_back(std::move(ancestor));
    if (parent == parent.root_path()) break;
  }
  devices.ancestor_count = static_cast<DWORD>(ancestors.size());
  devices.workspace_volume = workspace_id.VolumeSerialNumber;
  devices.workspace_drive = argv[1][0];
  for (DWORD i = 0; i < devices.ancestor_count; ++i)
    if (!DuplicateHandle(GetCurrentProcess(), ancestors[i].value, pi.hProcess,
                         &devices.ancestors[i], 0, FALSE, DUPLICATE_SAME_ACCESS))
      fail(L"metadata ancestor payload");
  if (!DuplicateHandle(GetCurrentProcess(), nul.value, pi.hProcess,
                       &devices.null_device, 0, FALSE, DUPLICATE_SAME_ACCESS) ||
      !DuplicateHandle(GetCurrentProcess(), kh.value, pi.hProcess,
                       &devices.crypto_device, 0, FALSE,
                       DUPLICATE_SAME_ACCESS) ||
      !DetourCopyPayloadToProcess(pi.hProcess, latch_handles_id, &devices,
                                  sizeof(devices)))
    fail(L"device payload");
  grant_appcontainer_namespace(sid, write_sid);
  wchar_t module[32768];
  const DWORD module_length = GetModuleFileNameW(nullptr, module, 32768);
  if (!module_length || module_length >= 32768) fail(L"module path");
  std::wstring hook(module, module_length);
  hook.resize(hook.find_last_of(L"\\/") + 1);
  hook += L"latch-boundary-compat.dll";
  const int count = WideCharToMultiByte(CP_UTF8, 0, hook.c_str(), -1, nullptr,
                                        0, nullptr, nullptr);
  std::string hook_utf8(static_cast<size_t>(count), 0);
  WideCharToMultiByte(CP_UTF8, 0, hook.c_str(), -1, hook_utf8.data(), count,
                      nullptr, nullptr);
  LPCSTR dll = hook_utf8.c_str();
  if (!DetourUpdateProcessWithDll(pi.hProcess, &dll, 1)) {
    TerminateProcess(pi.hProcess, 125);
    fail(L"inject");
  }
  cancel.check();
  recovery.execution_start();
  if (ResumeThread(pi.hThread) == static_cast<DWORD>(-1)) fail(L"resume child");
  recovery.pause(L"descendants");
  const HANDLE waits[] = {cancel.handle(), process.value};
  const DWORD waited = WaitForMultipleObjects(2, waits, FALSE, timeout_ms);
  if (waited == WAIT_FAILED) fail(L"wait child or launcher");
  DWORD code = 125;
  if (waited == WAIT_OBJECT_0 + 1) {
    if (!GetExitCodeProcess(process.value, &code)) fail(L"child exit code");
  } else if (waited == WAIT_TIMEOUT) {
    code = 124;
  } else if (waited != WAIT_OBJECT_0) {
    fail(L"unexpected child wait", ERROR_INVALID_DATA);
  }
  if (!job.stop(code)) fail(L"drain job");
  return static_cast<int>(code);
}
// The public launcher is the handle Latch owns. A separate trusted helper
// owns profiles, grants and the sandbox job, so killing the launcher still
// executes normal cleanup. It never executes a model command unrestricted.
int launch_owner(int argc, wchar_t** argv) {
  Handle lifetime;
  if (!DuplicateHandle(GetCurrentProcess(), GetCurrentProcess(),
                       GetCurrentProcess(), &lifetime.value, SYNCHRONIZE, TRUE,
                       0))
    fail(L"launcher lifetime");
  wchar_t module[32768]{};
  const DWORD length = GetModuleFileNameW(nullptr, module, 32768);
  if (!length || length >= 32768) fail(L"launcher path");
  std::wstring line =
      quote(std::wstring(module, length)) + L" --cleanup-owner " +
      std::to_wstring(reinterpret_cast<uintptr_t>(lifetime.value));
  for (int i = 1; i < argc; ++i) line += L" " + quote(argv[i]);
  Handle nul(CreateFileW(L"NUL", GENERIC_READ | GENERIC_WRITE,
                         FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                         OPEN_EXISTING, 0, nullptr));
  if (nul.value == INVALID_HANDLE_VALUE) fail(L"launcher NUL");
  Attributes attributes(1);
  STARTUPINFOEXW startup{};
  startup.StartupInfo.cb = sizeof(startup);
  startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
  startup.lpAttributeList = attributes.value;
  std::array<Handle, 3> stdio;
  std::array<HANDLE, 4> inherited{lifetime.value, nullptr, nullptr, nullptr};
  const DWORD sources[] = {STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                           STD_ERROR_HANDLE};
  HANDLE* outputs[] = {&startup.StartupInfo.hStdInput,
                       &startup.StartupInfo.hStdOutput,
                       &startup.StartupInfo.hStdError};
  for (size_t i = 0; i < stdio.size(); ++i) {
    HANDLE source = GetStdHandle(sources[i]);
    if (!source || source == INVALID_HANDLE_VALUE) source = nul.value;
    if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                         &stdio[i].value, 0, TRUE, DUPLICATE_SAME_ACCESS))
      fail(L"cleanup owner stdio");
    *outputs[i] = inherited[i + 1] = stdio[i].value;
  }
  if (!UpdateProcThreadAttribute(
          attributes.value, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
          inherited.data(), sizeof(inherited), nullptr, nullptr))
    fail(L"cleanup owner handles");
  PROCESS_INFORMATION created{};
  if (!CreateProcessW(module, line.data(), nullptr, nullptr, TRUE,
                      EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, nullptr,
                      nullptr, &startup.StartupInfo, &created))
    fail(L"create cleanup owner");
  Handle process(created.hProcess), thread(created.hThread);
  if (WaitForSingleObject(process.value, INFINITE) != WAIT_OBJECT_0)
    fail(L"wait cleanup owner");
  DWORD code = 125;
  if (!GetExitCodeProcess(process.value, &code)) fail(L"cleanup owner exit");
  return static_cast<int>(code);
}

}  // namespace latch
