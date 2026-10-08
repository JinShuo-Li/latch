#pragma once
#include <map>
#include <memory>

#include "common.h"
#include "object_security.h"

namespace latch {
struct AclChange {
  PinnedObject* pinned = nullptr;
  ObjectState before;
  std::wstring after;
};

class Recovery {
 public:
  explicit Recovery(const Cancellation& cancel);
  ~Recovery() = default;  // Failed rollback intentionally leaves the journal.
  Recovery(const Recovery&) = delete;
  Recovery& operator=(const Recovery&) = delete;
  void begin();

  void finish();
  void change(PinnedObject& pinned, const ObjectState& before, PACL acl,
              bool protect = false);
  void change_batch(const std::vector<AclChange>& changes);
  void track_root(const std::filesystem::path& path);
  bool protected_journal_path(const std::filesystem::path& path) const;
  Handle reserve_git(const std::filesystem::path& path);

  void prepare_profile();
  void profile_created();
  void execution_start();
  std::filesystem::path scratch_path() const;
  void pause(const wchar_t* point) const;
  const std::wstring& profile() const { return profile_; }
  const std::wstring& package_sid() const { return package_sid_; }
  const std::wstring& write_sid() const { return write_sid_; }
  const std::wstring& job_name() const { return job_; }
  const std::filesystem::path& root() const { return root_; }

 private:
  const Cancellation& cancel_;
  std::filesystem::path root_, pending_;
  std::vector<Handle> root_pins_;
  Handle lock_;

  std::wstring profile_, package_sid_, write_sid_, job_;
  unsigned sequence_ = 0;
  unsigned mutations_ = 0;
  void record(const std::vector<std::wstring>& fields);
  void record_batch(const std::vector<std::vector<std::wstring>>& rows);
  void recover_pending();
};
}  // namespace latch
