#pragma once

#include <cstddef>
#include <winsock2.h>

// Bounded, versioned messages on a host-created message pipe. The sandbox
// receives only its client endpoint and a transaction mutex, never a handle
// to the cleanup owner. The broker validates every request independently.
inline constexpr DWORD latch_broker_version = 2;
enum class LatchBrokerOperation : DWORD { read, socket_create, socket_release };
struct LatchReadRequest {
  DWORD version;
  DWORD process_id;
  ULONGLONG request_id;
  LatchBrokerOperation operation;
  int family, socket_type, protocol;
  DWORD socket_flags;
  ULONGLONG socket_ticket;
  ACCESS_MASK access;
  ULONG share;
  ULONG options;
  DWORD path_length;
  wchar_t path[32768];
};
struct LatchReadResponse {
  DWORD version;
  DWORD process_id;
  ULONGLONG request_id;
  DWORD error;
  HANDLE file;
  WSAPROTOCOL_INFOW socket_information;
  ULONGLONG socket_ticket;
};

inline DWORD latch_read_request_size(DWORD path_length) {
  return static_cast<DWORD>(offsetof(LatchReadRequest, path)) + path_length * sizeof(wchar_t);
}
