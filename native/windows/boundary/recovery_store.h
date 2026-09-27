#pragma once
#include "object_security.h"
namespace latch::recovery_store {
std::wstring current_user();
std::filesystem::path local_appdata();
std::wstring user_acl();
void durable_record(const std::filesystem::path& final,
                    const std::vector<std::wstring>& fields);
std::vector<std::wstring> read_record(const std::filesystem::path& path);
}  // namespace latch::recovery_store
