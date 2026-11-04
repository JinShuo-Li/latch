#!/usr/bin/env python3
"""Opt-in, paid LSP usability experiments with isolated workspaces/state.

Uses an existing OpenCode Go credential without copying it into configs or
reports. No provider traffic unless --live is explicitly supplied.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tomllib

from run import git_revision, init_workspace, report_root, run_process, save_json

REPO = Path(__file__).resolve().parent.parent
MODEL = "deepseek-v4.1-flash"
CODE = """namespace billing {
int quote(int n) { return n * 10; }
double quote(double n) { return n * 1.5; }
}
int main() {
  auto cents = billing::quote(3);
  auto fractional = billing::quote(2.0);
  return cents == 30 && fractional == 3.0 ? 0 : 1;
}
"""
NAVIGATION = ("Identify exactly which declaration and calls belong to the integer overload "
              "of billing::quote, excluding the floating overload. Report file and line "
              "numbers and its signature/return type. Use semantic navigation if available. "
              "Do not modify files. Explain any limitation rather than inventing results.")
REPAIR = ("The program fails to compile. Restore the intended behavior: three whole units "
          "must cost 36 cents, and the floating quote(2.0) must remain 3.0. Fix the invalid "
          "integer call and correct the integer pricing rate without changing the floating "
          "overload. Inspect language diagnostics if available, then validate that the "
          "program builds and runs. Diagnostic reports alone are not validation.")
CASES = [("overload_navigation", NAVIGATION, "normal"),
         ("diagnose_and_repair", REPAIR, "repair"),
         ("missing_server_fallback", NAVIGATION, "missing")]


def events(state):
    calls, outcomes, validations = [], {}, []
    with sqlite3.connect(state / "latch.sqlite3") as connection:
        for kind, payload in connection.execute("SELECT kind,payload FROM events ORDER BY rowid"):
            data = json.loads(payload).get("data", {})
            if kind == "tool_requested":
                calls.append(data["call"])
            elif kind in ("tool_completed", "tool_failed"):
                result = data["result"]
                outcomes[result["call_id"]] = result
            elif kind == "validation_result":
                validations.append(data)
    return calls, outcomes, validations


def credential(source, provider, secrets):
    scheme, name = provider["credential"].split(":", 1)
    if scheme == "env":
        value = os.environ.get(name)
    elif scheme == "file":
        if secrets is None:
            state = source.get("state_dir")
            if not state:
                raise ValueError("file credential needs --secrets when source state_dir is implicit")
            secrets = Path(state).expanduser() / "secrets.toml"
        value = tomllib.loads(secrets.read_text(encoding="utf-8")).get("secrets", {}).get(name)
    else:
        raise ValueError("live pilot supports env/file OpenCode Go credentials only")
    if not value:
        raise ValueError("configured credential could not be resolved")
    return value


def config_text(provider_id, provider, state, clangd):
    base = (f"base_url = {json.dumps(provider['base_url'])}\n" if provider.get("base_url") else "")
    return f"""state_dir = {json.dumps(str(state))}
[providers.{json.dumps(provider_id)}]
kind = "opencode-go"
credential = "env:LATCH_LSP_LIVE_KEY"
default_model = "{MODEL}"
{base}
[permissions]
mode = "auto_approve"
outside_workspace = "deny"
shell_timeout_seconds = 30
[failure]
max_model_turns = 12
retry_budget = 1
[[lsp_servers]]
name = "clangd"
command = {json.dumps(clangd)}
args = ["--background-index=false", "--clang-tidy=false", "--enable-config=false"]
extensions = ["cpp", "hpp"]
language_id = "cpp"
timeout_seconds = 30
"""


def independent_acceptance(case_dir, workspace):
    # Candidate code is compiled/executed only inside a credential-free, offline
    # Bubblewrap sandbox. The independent harness is never shown to the model.
    harness = case_dir / "acceptance"
    harness.mkdir()
    (harness / "main.cpp").write_text('''#define main latch_candidate_main
#include "/tmp/workspace/main.cpp"
#undef main
int main() {
  return billing::quote(3) == 36 && billing::quote(7) == 84 &&
    billing::quote(0) == 0 && billing::quote(2.0) == 3.0 &&
    billing::quote(4.0) == 6.0 && latch_candidate_main() == 0 ? 0 : 1;
}
''', encoding="utf-8")
    command = ["bwrap", "--die-with-parent", "--new-session", "--unshare-user", "--unshare-pid",
               "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try", "--unshare-net",
               "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp",
               "--tmpfs", "/run", "--tmpfs", "/home", "--ro-bind", str(workspace), "/tmp/workspace",
               "--ro-bind", str(harness), "/tmp/acceptance", "--clearenv", "--setenv", "HOME", "/tmp",
               "--setenv", "PATH", "/usr/bin:/bin", "--chdir", "/tmp/workspace", "--", "/bin/sh", "-c",
               "c++ /tmp/acceptance/main.cpp -o /tmp/check && /tmp/check"]
    outcome = run_process(command, cwd=workspace, timeout=30)
    save_json(case_dir / "acceptance.json", outcome)
    return outcome["exit_code"] == 0 and not outcome["timed_out"]


def execute(root, name, prompt, mode, options, provider, env):
    case = root / name
    case.mkdir(mode=0o700)
    workspace = case / "workspace"
    workspace.mkdir()
    text = CODE
    if mode == "repair":
        text = text.replace("billing::quote(3)", 'billing::quote("3")').replace("cents == 30", "cents == 36")
    (workspace / "main.cpp").write_text(text, encoding="utf-8")
    init_workspace(workspace)
    before = hashlib.sha256(text.encode()).hexdigest()
    state = case / "state"
    config = case / "config.toml"
    clangd = "/latch-intentionally-missing-language-server" if mode == "missing" else shutil.which("clangd")
    config.write_text(config_text(options.provider, provider, state, clangd), encoding="utf-8")
    config.chmod(0o600)
    outcome = run_process([str(options.latch), "run", "--config", str(config), "--workspace", str(workspace),
                           "--provider", options.provider, "--model", MODEL, "--mode", "WORK" if mode == "repair" else "ASK",
                           "--prompt", prompt, "--output", "json"], cwd=workspace, timeout=240, env=env)
    save_json(case / "process.json", outcome)
    calls, results, validations = events(state)
    lsp = [{"arguments": c["arguments"], "result": results.get(c["id"])} for c in calls if c["name"] == "lsp"]
    save_json(case / "lsp-trace.json", lsp)
    lsp_errors = sum(bool(c["result"] and c["result"]["is_error"]) for c in lsp)
    completed = outcome["exit_code"] == 0 and not outcome["timed_out"]
    if mode == "repair":
        accepted = independent_acceptance(case, workspace)
        useful = any(c["arguments"].get("action") == "diagnostics" and c["result"] and not c["result"]["is_error"] for c in lsp)
        useful = useful and any(v.get("passed") for v in validations)
    else:
        accepted = hashlib.sha256((workspace / "main.cpp").read_bytes()).hexdigest() == before
        useful = False
        if mode == "missing":
            useful = lsp_errors > 0 and any(c["name"] in ("search", "read_file") for c in calls)
        else:
            for call in lsp:
                if call["arguments"].get("action") == "references" and call["result"] and not call["result"]["is_error"]:
                    value = json.loads(call["result"]["output"])
                    lines = {item["range"]["start"]["line"] for item in value.get("items", [])}
                    useful |= {2, 6}.issubset(lines) and not lines.intersection({3, 7})
    try:
        final = json.loads(outcome["stdout"])
    except json.JSONDecodeError:
        final = {}
    summary = {"case": name, "completed": completed, "status": final.get("status"), "independent_acceptance": accepted,
               "useful_lsp_or_fallback": useful, "passed": completed and accepted and useful,
               "lsp_actions": [c["arguments"].get("action") for c in lsp], "lsp_errors": lsp_errors,
               "tool_calls": len(calls), "validation_passes": sum(bool(v.get("passed")) for v in validations),
               "elapsed_seconds": outcome["wall_seconds"], "usage": final.get("usage", {}).get("graph", {})}
    save_json(case / "summary.json", summary)
    print(json.dumps(summary, ensure_ascii=False), flush=True)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="explicitly authorize paid provider calls")
    parser.add_argument("--source-config", type=Path, required=True)
    parser.add_argument("--secrets", type=Path)
    parser.add_argument("--provider", default="opencode-go")
    parser.add_argument("--latch", type=Path, default=REPO / "target/release/latch")
    parser.add_argument("--output-dir", type=Path, default=REPO / "benchmark/runs/lsp")
    parser.add_argument("--case", action="append", choices=[case[0] for case in CASES], help="run only selected case(s)")
    options = parser.parse_args()
    if not options.live:
        parser.error("paid experiment requires --live; at most 3 cases × 12 model turns, 240s per case")
    for program in ("clangd", "c++", "bwrap", "git"):
        if not shutil.which(program):
            parser.error(f"missing prerequisite: {program}")
    options.latch = options.latch.resolve()
    source = tomllib.loads(options.source_config.read_text(encoding="utf-8"))
    provider = source.get("providers", {}).get(options.provider, {})
    if provider.get("kind") != "opencode-go":
        parser.error("selected source provider must be kind opencode-go")
    env = os.environ.copy()
    env["LATCH_LSP_LIVE_KEY"] = credential(source, provider, options.secrets)
    root, _ = report_root(options.output_dir, "lsp")
    selected = [case for case in CASES if not options.case or case[0] in options.case]
    save_json(root / "provenance.json", {"revision": git_revision(REPO), "model": MODEL,
              "binary_sha256": hashlib.sha256(options.latch.read_bytes()).hexdigest(),
              "provider": options.provider, "clangd": subprocess.check_output(["clangd", "--version"], text=True).splitlines()[0],
              "limits": {"cases":len(selected),"model_turns_per_case":12,"seconds_per_case":240}})
    print(f"Private evidence: {root}", flush=True)
    summaries = [execute(root, *case, options, provider, env) for case in selected]
    save_json(root / "summary.json", summaries)
    raise SystemExit(0 if all(s["passed"] for s in summaries) else 1)


if __name__ == "__main__":
    main()
