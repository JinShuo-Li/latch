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
std::wstring quote(const std::wstring& value);
}  // namespace latch
