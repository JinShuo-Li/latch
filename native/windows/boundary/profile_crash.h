#pragma once
#include "common.h"
namespace latch {
class Recovery;
// Fixture-only interception. No hook or environment control ships in Cargo.
class ProfileCrashHooks {
 public:
  explicit ProfileCrashHooks(const Recovery& recovery);
  ~ProfileCrashHooks();
  ProfileCrashHooks(const ProfileCrashHooks&) = delete;
  ProfileCrashHooks& operator=(const ProfileCrashHooks&) = delete;
};
}  // namespace latch
