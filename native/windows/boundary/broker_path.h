#pragma once
#include "common.h"

namespace latch {
inline bool broker_path_within(const std::wstring& path, const std::wstring& root) {
  return _wcsicmp(path.c_str(), root.c_str()) == 0 ||
      (path.size() > root.size() &&
       _wcsnicmp(path.c_str(), root.c_str(), root.size()) == 0 &&
       path[root.size()] == L'\\');
}
inline std::wstring broker_normalize(const std::wstring& input) {
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
}  // namespace latch
