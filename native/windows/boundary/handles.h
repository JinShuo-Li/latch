#pragma once

// A Detours payload carries metadata-only ancestor handles as well as the
// two device handles when a developer tool creates a child without inheritance.
inline constexpr GUID latch_handles_id = {
  0x79b8100a, 0x4844, 0x4c4a, {0x84, 0x76, 0xb4, 0x02, 0x60, 0x9b, 0x97, 0xee}
};
inline constexpr size_t latch_max_ancestors = 64;
struct LatchHandles {
  HANDLE null_device;
  HANDLE crypto_device;
  HANDLE read_broker;
  HANDLE broker_mutex;
  ULONGLONG workspace_volume;
  wchar_t workspace_drive;
  DWORD ancestor_count;
  HANDLE ancestors[latch_max_ancestors];
};
