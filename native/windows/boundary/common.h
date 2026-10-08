#pragma once
#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif
#define NOMINMAX
#define WIN32_LEAN_AND_MEAN
#include <aclapi.h>
#include <rpc.h>
#include <sddl.h>
#include <windows.h>
#include <winternl.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cwctype>
#include <filesystem>
#include <set>
#include <string>
#include <string_view>
#include <vector>

namespace latch {
struct Error {
  const wchar_t* api;
  DWORD code;
};

[[noreturn]] inline void fail(const wchar_t* api, DWORD code = GetLastError()) {
  throw Error{api, code};
}

inline void require(bool condition, const wchar_t* message) {
  if (!condition) fail(message, ERROR_INVALID_DATA);
}

struct Handle {
  HANDLE value = nullptr;
  ~Handle() {
    if (value != nullptr && value != INVALID_HANDLE_VALUE) CloseHandle(value);
  }
  Handle() = default;
  explicit Handle(HANDLE input) : value(input) {}
  Handle(const Handle&) = delete;
  Handle& operator=(const Handle&) = delete;
  Handle(Handle&& other) noexcept : value(other.value) {
    other.value = nullptr;
  }
  Handle& operator=(Handle&&) = delete;
};

struct Local {
  void* value = nullptr;
  ~Local() {
    if (value != nullptr) LocalFree(value);
  }
  Local() = default;
  explicit Local(void* input) : value(input) {}
  Local(const Local&) = delete;
  Local& operator=(const Local&) = delete;
};

class Cancellation {
 public:
  explicit Cancellation(HANDLE handle = nullptr) : handle_(handle) {}
  void check() const;
  HANDLE handle() const { return handle_; }

 private:
  HANDLE handle_;
};

// Opt-in profiling for native Windows boundary acceptance fixtures. Grant
// walk includes ACL updates and journal writes; both are also reported
// separately so large-workspace cost can be attributed.
struct BoundaryTiming {
  bool enabled = false;
  std::chrono::nanoseconds preflight_scan{};
  std::chrono::nanoseconds grant_walk{};
  std::chrono::nanoseconds journal{};
  std::chrono::nanoseconds acl_apply{};
  std::chrono::nanoseconds rollback{};
  uint64_t journal_records = 0;
  uint64_t acl_mutations = 0;
};

BoundaryTiming& boundary_timing();
void configure_boundary_timing();
void report_boundary_timing();

class TimingScope {
 public:
  explicit TimingScope(std::chrono::nanoseconds& total);
  ~TimingScope();
  TimingScope(const TimingScope&) = delete;
  TimingScope& operator=(const TimingScope&) = delete;
  void stop();

 private:
  std::chrono::nanoseconds* total_;
  std::chrono::steady_clock::time_point started_;
};

std::wstring quote(const std::wstring& value);
}  // namespace latch
