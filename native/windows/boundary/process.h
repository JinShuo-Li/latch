#pragma once
#include "common.h"
#include "recovery.h"
namespace latch {
struct Attributes {
  std::vector<BYTE> storage;
  LPPROC_THREAD_ATTRIBUTE_LIST value = nullptr;
  explicit Attributes(DWORD count) {
    SIZE_T size = 0;
    InitializeProcThreadAttributeList(nullptr, count, 0, &size);
    storage.resize(size);
    auto* list = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    if (!InitializeProcThreadAttributeList(list, count, 0, &size))
      fail(L"initialize process attributes");
    value = list;
  }
  ~Attributes() {
    if (value) DeleteProcThreadAttributeList(value);
  }
  Attributes(const Attributes&) = delete;
  Attributes& operator=(const Attributes&) = delete;
};

int launch_owner(int argc, wchar_t** argv);
int execute_target(wchar_t** argv, const Cancellation& cancel,
                   HANDLE restricted, PSID write_sid, PSID sid,
                   LPPROC_THREAD_ATTRIBUTE_LIST attrs, DWORD timeout_ms,
                   Recovery& recovery,
                   const std::vector<std::wstring>& read_roots,
                   const std::vector<std::wstring>& denied_roots);
}  // namespace latch
