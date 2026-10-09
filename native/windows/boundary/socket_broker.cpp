#include "socket_broker.h"

namespace latch {
SocketBroker::SocketBroker(bool enabled) : enabled_(enabled) {
  if (enabled_) {
    WSADATA data{};
    const int error = WSAStartup(MAKEWORD(2, 2), &data);
    if (error) fail(L"initialize socket broker", static_cast<DWORD>(error));
  }
}
SocketBroker::~SocketBroker() {
  for (const auto& [ticket, pending] : pending_) {
    (void)ticket;
    closesocket(pending.socket);
  }
  if (enabled_) WSACleanup();
}
DWORD SocketBroker::create(DWORD process_id, int family, int type, int protocol,
                           DWORD flags, WSAPROTOCOL_INFOW& information,
                           ULONGLONG& ticket) {
  if (!enabled_) return WSAEACCES;
  if ((family != AF_INET && family != AF_INET6) ||
      (type != SOCK_STREAM && type != SOCK_DGRAM) ||
      (protocol != 0 && protocol != (type == SOCK_STREAM ? IPPROTO_TCP : IPPROTO_UDP)) ||
      (flags & ~(WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT)))
    return WSAEACCES;
  for (auto current = pending_.begin(); current != pending_.end();) {
    if (WaitForSingleObject(current->second.process.value, 0) == WAIT_OBJECT_0) {
      closesocket(current->second.socket);
      current = pending_.erase(current);
    } else ++current;
  }
  if (pending_.size() >= 64) return WSAENOBUFS;
  Handle process(OpenProcess(SYNCHRONIZE, FALSE, process_id));
  if (!process.value) return WSAEACCES;
  const SOCKET socket = WSASocketW(family, type, protocol, nullptr, 0,
                                  flags | WSA_FLAG_NO_HANDLE_INHERIT);
  if (socket == INVALID_SOCKET) return static_cast<DWORD>(WSAGetLastError());
  if (WSADuplicateSocketW(socket, process_id, &information)) {
    const DWORD error = static_cast<DWORD>(WSAGetLastError());
    closesocket(socket);
    return error;
  }
  ticket = ++sequence_;
  pending_.emplace(ticket, Pending{process_id, socket, std::move(process)});
  return ERROR_SUCCESS;
}
DWORD SocketBroker::release(DWORD process_id, ULONGLONG ticket) {
  const auto found = pending_.find(ticket);
  if (found == pending_.end() || found->second.process_id != process_id)
    return WSAEACCES;
  closesocket(found->second.socket);
  pending_.erase(found);
  return ERROR_SUCCESS;
}
}  // namespace latch
