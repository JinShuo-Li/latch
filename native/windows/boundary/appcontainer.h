#pragma once
#include "common.h"
namespace latch {
class Recovery;
// SID lifetime is RAII; durable profile ownership belongs to Recovery.
struct AppContainer {
  PSID sid = nullptr;
  explicit AppContainer(Recovery& recovery);
  ~AppContainer();
  AppContainer(const AppContainer&) = delete;
  AppContainer& operator=(const AppContainer&) = delete;
};

void grant_appcontainer_namespace(PSID sid, PSID write_sid);
// Returns false only when the SID registration is absent. A registration with
// a different moniker is a conflict, never authority to delete a profile.
bool validate_profile_mapping(const std::wstring& profile,
                              const std::wstring& sid, bool must_be_absent);
// Removes only the empty SID key left before the profile API writes Moniker.
// The caller must hold a durable absence-checked creation intent, and prove
// that no package directory exists. No subtree deletion is permitted here.
bool remove_empty_profile_mapping(const std::wstring& sid);
}  // namespace latch
