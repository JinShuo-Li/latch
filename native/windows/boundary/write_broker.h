#pragma once
#include "common.h"
#include "object_security.h"
#include "broker_protocol.h"

namespace latch {
class WriteBroker {
 public:
  WriteBroker(const std::vector<std::wstring>& roots,
              const std::vector<std::wstring>& denied,
              const std::vector<std::wstring>& denied_write,
              const std::vector<std::wstring>& protected_git);
  void open(const LatchReadRequest& request, LatchReadResponse& response, HANDLE process);
  void rename(const LatchReadRequest& request, LatchReadResponse& response, HANDLE process);
 private:
  std::vector<std::wstring> roots_, denied_, git_;
  std::vector<PinnedObject> pins_;
  bool allowed(const std::wstring& path) const;
  void check_parent(const PinnedObject& parent, const std::wstring& path) const;
  void check(HANDLE file, const PinnedObject& parent, const std::wstring& path) const;
};
}  // namespace latch
