#include "common.h"
namespace latch {
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
