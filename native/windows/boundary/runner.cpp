#include "common.h"

#include <objbase.h>
#include <userenv.h>

#include "appcontainer.h"
#include "filesystem_acl.h"
#include "process.h"
#include "token.h"
namespace latch {
int run_boundary(int argc, wchar_t** argv, const Cancellation& cancel) {
  configure_boundary_timing();
  cancel.check();
  SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX |
               SEM_NOOPENFILEERRORBOX);
  Recovery recovery(cancel);
  recovery.begin();
  int result = 125;
  try {
    AppContainer container(recovery);
    PSID sid = container.sid;
    recovery.profile_created();
    Grants grants(sid, cancel, recovery);
    Local write_sid = parse_sid(recovery.write_sid().c_str());
    Grants write_grants(write_sid.value, cancel, recovery);
    // Remove AppContainer read grants on sensitive paths; package-specific
    // deny ACEs do not suppress the All Application Packages allow route.
    std::set<std::wstring> protected_paths;
    GitReservation git_reservation;
    DWORD timeout_ms = INFINITE;

    constexpr DWORD read_rights = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    constexpr DWORD write_rights = FILE_GENERIC_WRITE | DELETE;
    bool workspace_writable = std::wcscmp(argv[4], L"write") == 0;
    std::vector<std::wstring> grant_roots{
        std::filesystem::canonical(argv[1]).wstring()};
    std::vector<std::wstring> read_roots;
    for (int i = 5; i < argc; i += 2) {
      if (i + 1 >= argc) fail(L"missing option path", ERROR_INVALID_PARAMETER);
      if (std::wcscmp(argv[i], L"--write-root") == 0) {
        const auto root = std::filesystem::canonical(argv[i + 1]).wstring();
        const auto existing = std::find_if(
            grant_roots.begin(), grant_roots.end(),
            [&](const std::wstring& value) {
              return _wcsicmp(value.c_str(), root.c_str()) == 0;
            });
        if (existing == grant_roots.begin())
          workspace_writable = true;
        else if (existing == grant_roots.end())
          grant_roots.push_back(root);
      } else if (std::wcscmp(argv[i], L"--read-root") == 0) {
        const auto root = std::filesystem::canonical(argv[i + 1]).wstring();
        const auto already_granted = std::any_of(
            grant_roots.begin(), grant_roots.end(),
            [&](const std::wstring& value) {
              return _wcsicmp(value.c_str(), root.c_str()) == 0;
            });
        const auto already_read = std::any_of(
            read_roots.begin(), read_roots.end(),
            [&](const std::wstring& value) {
              return _wcsicmp(value.c_str(), root.c_str()) == 0;
            });
        if (!already_granted && !already_read) read_roots.push_back(root);
      }
    }
    std::vector<std::wstring> allowed_roots = grant_roots;
    allowed_roots.insert(allowed_roots.end(), read_roots.begin(),
                         read_roots.end());
    std::vector<std::wstring> writable_roots = grant_roots;
    if (!workspace_writable) writable_roots.erase(writable_roots.begin());
    std::vector<std::wstring> denied_roots{recovery.root().wstring()};
    for (int i = 5; i + 1 < argc; i += 2)
      if (std::wcscmp(argv[i], L"--deny") == 0)
        denied_roots.push_back(std::filesystem::absolute(argv[i + 1]).lexically_normal().wstring());
    std::vector<GrantPlan> grant_plans;
    grant_plans.reserve(allowed_roots.size());
    {
      TimingScope timer(boundary_timing().preflight_scan);
      for (const auto& root : allowed_roots) {
        grant_plans.emplace_back();
        const bool writable = std::any_of(
            writable_roots.begin(), writable_roots.end(),
            [&](const std::wstring& value) {
              return _wcsicmp(value.c_str(), root.c_str()) == 0;
            });
        if (!workspace_writable && _wcsicmp(root.c_str(), grant_roots.front().c_str()) == 0)
          continue;  // Workspace read opens are mediated, without ACL grants.
        validate_grant_tree(root, writable ? writable_roots : allowed_roots,
                            grant_plans.back(), cancel,
                            recovery, writable);
      }
    }
    for (const auto& root : allowed_roots)
      if (workspace_writable || _wcsicmp(root.c_str(), grant_roots.front().c_str()) != 0)
        recovery.track_root(root);
    TimingScope grant_timer(boundary_timing().grant_walk);
    for (size_t index = 0; index < grant_roots.size(); ++index) {
      if (index == 0 && !workspace_writable) continue;
      const DWORD rights =
          read_rights | ((index == 0 && !workspace_writable) ? 0 : write_rights);
      grants.add_plan(grant_plans[index],
                      (index != 0 || workspace_writable) ? writable_roots : allowed_roots,
                      write_sid.value,
                      rights);
    }
    for (size_t index = 0; index < read_roots.size(); ++index)
      grants.add_plan(grant_plans[grant_roots.size() + index], allowed_roots,
                      nullptr, read_rights);
    Attributes attributes(3);
    auto* attrs = attributes.value;
    Local internet = parse_sid(L"S-1-15-3-1");
    Local private_network = parse_sid(L"S-1-15-3-3");
    SID_AND_ATTRIBUTES network_caps[] = {
        {internet.value, SE_GROUP_ENABLED},
        {private_network.value, SE_GROUP_ENABLED}};
    SECURITY_CAPABILITIES caps{};
    caps.AppContainerSid = sid;
    for (int i = 5; i + 1 < argc; i += 2) {
      if (std::wcscmp(argv[i], L"--network") == 0 &&
          std::wcscmp(argv[i + 1], L"yes") == 0) {
        caps.Capabilities = network_caps;
        caps.CapabilityCount = 2;
      }
    }
    bool package_boundary = true;
#ifdef LATCH_RECOVERY_TESTING
    wchar_t diagnostic[2]{};
    package_boundary = GetEnvironmentVariableW(L"LATCH_DIAG_NO_APPCONTAINER",
                                                diagnostic, 2) == 0;
#endif
    if (package_boundary &&
        !UpdateProcThreadAttribute(attrs, 0,
                                   PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                                   &caps, sizeof(caps), nullptr, nullptr))
      fail(L"Update attrs");
    // Windows redirects GetTempPath inside an AppContainer to this private
    // per-call directory, regardless of TEMP/TMP. Grant only this scratch tree.
    const auto scratch = recovery.scratch_path();
    std::filesystem::create_directories(scratch);
    grants.add_pair(scratch.wstring(), write_sid.value,
                    read_rights | write_rights);
    for (int i = 5; i < argc; ++i) {
      const std::wstring option = argv[i++];
      if (i >= argc) fail(L"missing option path", ERROR_INVALID_PARAMETER);
      if (option == L"--read-root") {
        // All read roots were tracked and granted before option processing.
      } else if (option == L"--network") {
        if (std::wcscmp(argv[i], L"yes") != 0 &&
            std::wcscmp(argv[i], L"no") != 0)
          fail(L"invalid network mode", ERROR_INVALID_PARAMETER);
      } else if (option == L"--timeout-ms") {
        wchar_t* end = nullptr;
        const unsigned long value = std::wcstoul(argv[i], &end, 10);
        if (!end || *end || value == 0 || value >= INFINITE)
          fail(L"invalid timeout", ERROR_INVALID_PARAMETER);
        timeout_ms = value;
      } else if (option == L"--protect-git") {
        if (workspace_writable) git_reservation.create(argv[i], recovery);
      } else if (option == L"--write-root") {
        // All write roots were tracked and granted before option processing.
      } else if (option == L"--deny") {
        const bool audit_only = writable_roots.empty();
        protect_sensitive_tree(argv[i], protected_paths, cancel, recovery, audit_only);
        if (!audit_only) {
          grants.add(argv[i], DENY_ACCESS, FILE_ALL_ACCESS);
          write_grants.add(argv[i], DENY_ACCESS, FILE_ALL_ACCESS);
        }
      } else if (option == L"--deny-write") {
        if (writable_roots.empty()) continue;
        constexpr DWORD mutate = FILE_WRITE_DATA | FILE_APPEND_DATA |
                                 FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES |
                                 DELETE | WRITE_DAC | WRITE_OWNER |
                                 FILE_DELETE_CHILD;
        grants.add(argv[i], DENY_ACCESS, mutate);
        write_grants.add(argv[i], DENY_ACCESS, mutate);
      } else
        fail(L"invalid option", ERROR_INVALID_PARAMETER);
    }
    grant_timer.stop();
    recovery.pause(L"all-grants");
    Handle restricted = restrict_token(write_sid.value);
    result = execute_target(argv, cancel, restricted.value, write_sid.value,
                            sid, attrs, timeout_ms, recovery, allowed_roots,
                            denied_roots);

  } catch (const Error& e) {
    std::fwprintf(stderr, L"%ls: %lu\n", e.api, e.code);
  } catch (const std::exception& e) {
    std::fprintf(stderr, "Windows boundary: %s", e.what());
  }

  {
    TimingScope timer(boundary_timing().rollback);
    recovery.finish();
  }
  report_boundary_timing();
  return result;
}

}  // namespace latch
using namespace latch;
int wmain(int argc, wchar_t** argv) {
  SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX |
               SEM_NOOPENFILEERRORBOX);
  try {
    if (argc >= 3 && std::wcscmp(argv[1], L"--cleanup-owner") == 0) {
      wchar_t* end = nullptr;
      const auto raw = _wcstoui64(argv[2], &end, 10);
      if (!raw || !end || *end) return 2;
      Handle owner(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(raw)));
      Cancellation cancel(owner.value);
      cancel.check();
      if (argc < 7) return 2;
      return run_boundary(argc - 2, argv + 2, cancel);
    }
    if (argc == 2 && std::wcscmp(argv[1], L"--recover-only") == 0) {
      configure_boundary_timing();
      Cancellation cancel;
      Recovery recovery(cancel);
      {
        TimingScope timer(boundary_timing().rollback);
        recovery.finish();
      }
      report_boundary_timing();
      return 0;
    }
    if (argc < 5) return 2;
    return launch_owner(argc, argv);
  } catch (const Error& error) {
    std::fwprintf(stderr, L"%ls: %lu\n", error.api, error.code);
  } catch (const std::exception& error) {
    std::fprintf(stderr, "Windows launcher: %s\n", error.what());
  }
  return 125;
}
