#pragma once
#include <winsock2.h>
#include <map>
#include "common.h"

namespace latch {
// Network capability transfers ordinary TCP/UDP sockets into this call's
// authenticated processes. It never changes firewall or loopback policy.
class SocketBroker {
 public:
  explicit SocketBroker(bool enabled);
  ~SocketBroker();
  DWORD create(DWORD process_id, int family, int type, int protocol,
               DWORD flags, WSAPROTOCOL_INFOW& information, ULONGLONG& ticket);
  DWORD release(DWORD process_id, ULONGLONG ticket);
 private:
  struct Pending { DWORD process_id; SOCKET socket; Handle process; };
  bool enabled_;
  ULONGLONG sequence_ = 0;
  std::map<ULONGLONG, Pending> pending_;
};
}  // namespace latch
