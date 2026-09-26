#pragma once

// A Detours payload carries the two compatibility handles when a developer
// tool creates a child with bInheritHandles=FALSE or replaces its environment.
inline constexpr GUID latch_handles_id = {
  0x79b8100a, 0x4844, 0x4c4a, {0x84, 0x76, 0xb4, 0x02, 0x60, 0x9b, 0x97, 0xee}
};
struct LatchHandles {
  HANDLE null_device;
  HANDLE crypto_device;
};
