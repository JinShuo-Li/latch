#include "common.h"
namespace latch {
BoundaryTiming& boundary_timing() {
  static BoundaryTiming timing;
  return timing;
}

void configure_boundary_timing() {
  wchar_t value[2]{};
  const DWORD length = GetEnvironmentVariableW(
      L"LATCH_BOUNDARY_TIMING", value, 2);
  boundary_timing().enabled = length == 1 && value[0] == L'1';
}

TimingScope::TimingScope(std::chrono::nanoseconds& total)
    : total_(boundary_timing().enabled ? &total : nullptr),
      started_(total_ ? std::chrono::steady_clock::now()
                      : std::chrono::steady_clock::time_point{}) {}

TimingScope::~TimingScope() { stop(); }

void TimingScope::stop() {
  if (!total_) return;
  *total_ += std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now() - started_);
  total_ = nullptr;
}

void report_boundary_timing() {
  const auto& timing = boundary_timing();
  if (!timing.enabled) return;
  const auto millis = [](std::chrono::nanoseconds value) {
    return std::chrono::duration<double, std::milli>(value).count();
  };
  std::fprintf(stderr,
               "LATCH_BOUNDARY_TIMING preflight_scan_ms=%.3f "
               "grant_walk_ms=%.3f journal_ms=%.3f acl_apply_ms=%.3f "
               "rollback_ms=%.3f journal_records=%llu acl_mutations=%llu\n",
               millis(timing.preflight_scan), millis(timing.grant_walk),
               millis(timing.journal), millis(timing.acl_apply),
               millis(timing.rollback),
               static_cast<unsigned long long>(timing.journal_records),
               static_cast<unsigned long long>(timing.acl_mutations));
}

void Cancellation::check() const {
  if (!handle_) return;
  const DWORD status = WaitForSingleObject(handle_, 0);
  if (status == WAIT_OBJECT_0) fail(L"launcher cancelled", ERROR_CANCELLED);
  if (status != WAIT_TIMEOUT) fail(L"wait launcher");
}

std::wstring quote(const std::wstring& value) {
  std::wstring result = L"\"";
  size_t slashes = 0;
  for (wchar_t character : value) {
    if (character == L'\\') {
      ++slashes;
      continue;
    }
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

}  // namespace latch
