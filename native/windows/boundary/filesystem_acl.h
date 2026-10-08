#pragma once
#include "common.h"
#include "recovery.h"
namespace latch {
DWORD update_acl(const std::wstring& path, PSID sid, ACCESS_MODE mode,
                 DWORD rights, DWORD inheritance, const Cancellation& cancel,
                 Recovery& recovery);
struct GrantTarget {
  uint32_t relative_path_offset = 0;
  uint16_t relative_path_length = 0;
  DWORD attributes = 0;
  FILE_ID_INFO file_id{};
  LONGLONG creation_time = 0;
};

struct GrantPlan {
  std::filesystem::path root;
  // One flat UTF-16 arena avoids one heap allocation per object in large roots.
  std::wstring relative_paths;
  std::vector<GrantTarget> targets;
};

class Grants {
 public:
  Grants(PSID sid, const Cancellation& cancellation, Recovery& journal)
      : cancel(cancellation), sid_(sid), recovery(journal) {}
  void add(const std::wstring& path, ACCESS_MODE mode, DWORD rights);
  void add_pair(const std::wstring& path, PSID other_sid, DWORD rights);
  void add_plan(GrantPlan& plan, const std::vector<std::wstring>& allowed,
                PSID other_sid, DWORD rights);
  void add_one(const std::wstring& path, ACCESS_MODE mode, DWORD rights);

 private:
  const Cancellation& cancel;
  PSID sid_;
  Recovery& recovery;
  std::set<std::wstring> denied_;
};

struct GitReservation {
  Handle handle;
  void create(const std::filesystem::path& workspace, Recovery& recovery);
};

void validate_grant_tree(const std::filesystem::path& path,
                         const std::vector<std::wstring>& allowed,
                         GrantPlan& plan, const Cancellation& cancel,
                         Recovery& recovery);
void protect_sensitive_tree(const std::filesystem::path& input,
                            std::set<std::wstring>& visited,
                            const Cancellation& cancel, Recovery& recovery);
}  // namespace latch
