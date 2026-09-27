#include "job.h"
namespace latch {
Job::Job(const std::wstring& name) {
  handle.value = CreateJobObjectW(nullptr, name.c_str());
  if (GetLastError() == ERROR_ALREADY_EXISTS)
    fail(L"sandbox job name collision", ERROR_ALREADY_EXISTS);
  if (!handle.value) fail(L"create job");
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  if (!SetInformationJobObject(handle.value, JobObjectExtendedLimitInformation,
                               &limits, sizeof(limits)))
    fail(L"job limits");
}
bool Job::stop(DWORD exit_code) noexcept {
  if (stopped) return true;
  if (!TerminateJobObject(handle.value, exit_code)) return false;
  JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting{};
  for (;;) {
    if (!QueryInformationJobObject(handle.value,
                                   JobObjectBasicAccountingInformation,
                                   &accounting, sizeof(accounting), nullptr))
      return false;
    if (accounting.ActiveProcesses == 0) {
      stopped = true;
      return true;
    }
    Sleep(1);
  }
}
Job::~Job() {
  if (!stop(125)) {
    // Do not unwind into ACL revocation while ownership is uncertain.
    // Process exit closes the job; crash recovery is a separate gate.
    std::fwprintf(stderr,
                  L"windows runner: cannot confirm job teardown (%lu)\n",
                  GetLastError());
    ExitProcess(125);
  }
}
void drain_stale_job(const std::wstring& name) {
  Handle job(OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, FALSE,
                            name.c_str()));
  if (!job.value) {
    if (GetLastError() == ERROR_FILE_NOT_FOUND) return;
    fail(L"open stale sandbox job");
  }
  if (!TerminateJobObject(job.value, 125)) fail(L"terminate stale sandbox job");
  JOBOBJECT_BASIC_ACCOUNTING_INFORMATION info{};
  do {
    if (!QueryInformationJobObject(job.value,
                                   JobObjectBasicAccountingInformation, &info,
                                   sizeof(info), nullptr))
      fail(L"wait stale sandbox descendants");
    if (info.ActiveProcesses) Sleep(1);
  } while (info.ActiveProcesses);
}

}  // namespace latch
