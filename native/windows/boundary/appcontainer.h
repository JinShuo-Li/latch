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
}  // namespace latch
