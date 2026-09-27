#pragma once
#include "common.h"
namespace latch {
// The job drains before rollback. A failed drain exits with the journal intact.
struct Job {
  Handle handle;
  explicit Job(const std::wstring& name);
  bool stop(DWORD exit_code) noexcept;
  ~Job();

 private:
  bool stopped = false;
};
void drain_stale_job(const std::wstring& name);
}  // namespace latch
