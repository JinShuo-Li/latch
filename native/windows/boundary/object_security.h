#pragma once
#include "common.h"
namespace latch {
struct ObjectState {
  std::wstring path;
  std::wstring identity;
  std::wstring security;
};
// Opens each ancestor without delete sharing, rejects reparses, and keeps the
// target handle for both the identity check and security mutation (no reopen).
struct PinnedObject {
  std::vector<Handle> ancestors;
  Handle object;
  explicit PinnedObject(const std::filesystem::path& path,
                        DWORD access = READ_CONTROL | WRITE_DAC |
                                       FILE_READ_ATTRIBUTES);
  // Recovery after execution may reopen a renamed file by exact NTFS ID.
  // A null object means the original ID no longer exists, not a path miss.
  explicit PinnedObject(const ObjectState& original);
  ObjectState state() const;
};
std::wstring read_security(HANDLE handle);
std::wstring identity(HANDLE handle);
void write_security(HANDLE handle, const std::wstring& descriptor);
std::wstring changed_dacl(const std::wstring& original, PACL acl, bool protect);

Local descriptor(const std::wstring& text);
}  // namespace latch
