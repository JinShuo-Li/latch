#pragma once
#include "common.h"
namespace latch {
struct PrivateDesktop {
  HDESK desktop = nullptr;
  std::wstring name;
  PrivateDesktop() = default;
  PrivateDesktop(const PrivateDesktop&) = delete;
  PrivateDesktop& operator=(const PrivateDesktop&) = delete;
  ~PrivateDesktop();
  void create(PSID unique, const std::wstring& unique_name);
  void grant_package(PSID sid);
};
}  // namespace latch
