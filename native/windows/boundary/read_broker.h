#pragma once
#include <atomic>
#include <thread>
#include "common.h"
#include "object_security.h"

namespace latch {
// Read capabilities are mediated at open time. No source ACL is changed, and
// removing the DLL does not grant filesystem access to a restricted process.
class ReadBroker {
 public:
  ReadBroker(HANDLE job, PSID package, const std::vector<std::wstring>& roots,
             const std::vector<std::wstring>& denied);
  ~ReadBroker();
  ReadBroker(const ReadBroker&) = delete;
  ReadBroker& operator=(const ReadBroker&) = delete;
  HANDLE client() const { return client_.value; }
  HANDLE mutex() const { return mutex_.value; }
 private:
  HANDLE job_;
  std::vector<BYTE> package_;
  std::vector<std::wstring> roots_, denied_;
  std::vector<PinnedObject> roots_pinned_;
  Handle server_, client_, mutex_;
  std::atomic<bool> stopping_{false};
  std::thread thread_;
  void serve();
  bool allowed(const std::wstring& path) const;
  void check_links(HANDLE file, const std::wstring& path) const;
};
}  // namespace latch
