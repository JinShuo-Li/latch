# Install Latch

Latch 0.3.1 supports Linux and Windows. Official precompiled releases target
x86_64; macOS, ARM64, and musl/Alpine binaries are not currently provided.
The binary is named `latch` on Linux and `latch.exe` on Windows.

## Install a binary

The installers download the latest stable GitHub Release, require its
`SHA256SUMS`, and verify the archive before replacing an existing installation.
Latest-release lookup follows GitHub's public release redirect, without using
the GitHub API or requiring a token. The one-line commands work without pinning
a version, including on networks that have exhausted the unauthenticated API quota.
No root or administrator access is required. Review the scripts by downloading
[install.sh](../scripts/install.sh) or [install.ps1](../scripts/install.ps1)
first if you prefer to inspect them before running.

Linux, from Bash:

```sh
curl -fsSL https://jinshuo-li.github.io/latch/install.sh | bash
```

Windows, from Windows PowerShell 5.1+ or PowerShell 7:

```powershell
irm https://jinshuo-li.github.io/latch/install.ps1 | iex
```

Linux installs to `~/.local/bin`. Windows installs to
`%LOCALAPPDATA%\Programs\Latch\bin`. The scripts print PATH instructions if
needed; they do not change shell profiles or the persistent Windows PATH.
On Linux, add `export PATH="$HOME/.local/bin:$PATH"` to your shell profile.
On Windows, add the install directory to your **user** Path in Environment
Variables and reopen your terminal. Close Latch before upgrading on Windows.

Official Linux and Windows binary assets are available from v0.2.3. The Linux
installer also supports checksum-listed older versioned archives. The glibc 2.35
baseline applies to new workflow builds; older Linux archives may require the
builder’s newer glibc.

### Pin a version or choose a directory

For a published release such as `v0.3.1` (replace with an actual release tag):

```sh
curl -fsSL https://jinshuo-li.github.io/latch/install.sh | bash -s -- --version v0.3.1 --install-dir "$HOME/.local/bin"
```

Download the Windows script and pass its parameters:

```powershell
Invoke-WebRequest https://jinshuo-li.github.io/latch/install.ps1 -OutFile install.ps1
./install.ps1 -Version v0.3.1 -InstallDir "$env:LOCALAPPDATA\Programs\Latch\bin"
```

Both scripts also accept `LATCH_VERSION` and `LATCH_INSTALL_DIR` environment
variables. Omit the version or use `latest` for the latest stable release;
prereleases must be pinned explicitly. Running the installer again upgrades
the binary and preserves configuration and sessions in `~/.latch/`.

### Manual download

Download the archive for your platform and `SHA256SUMS` from the same
[release](https://github.com/JinShuo-Li/latch/releases):

| Platform | Asset | Runtime baseline |
| --- | --- | --- |
| Linux | `latch-x86_64-unknown-linux-gnu.tar.gz` | glibc 2.35+ (Ubuntu 22.04+, Debian 12+, or equivalent) |
| Windows | `latch-x86_64-pc-windows-msvc.zip` | x64 Windows with AppContainer support and an NTFS workspace |

Verify the archive hash against its exact filename in `SHA256SUMS`, using
`sha256sum` on Linux or `Get-FileHash -Algorithm SHA256` on Windows, then extract
`latch` / `latch.exe` to a directory on PATH. Archives also contain the MIT
license, an example configuration, and build provenance in `BUILD_INFO`.
Checksums detect corrupt or mismatched downloads; they are not a separate
signature authenticating the publisher.

## Runtime prerequisites

Linux requires Git, ripgrep (`rg`), and Bubblewrap (`bwrap`) on PATH, with
unprivileged user namespaces enabled. Debian / Ubuntu:

```sh
sudo apt update && sudo apt install bubblewrap ripgrep git
```

Windows requires Git for Windows and ripgrep on PATH; for example with winget:

```powershell
winget install --id Git.Git -e
winget install --id BurntSushi.ripgrep.MSVC -e
```

Reopen the terminal after installing these packages. The AppContainer sandbox
runner and compatibility helper are embedded in `latch.exe`, with a statically
linked C runtime. There is no unsandboxed fallback on either platform.

On Windows, use a focused NTFS checkout readable/modifiable by your account.
Ordinary source reads and writes use scoped file mediation without changing
source ACLs or requiring `WRITE_DAC`. Sensitive objects with broad/package read
grants still require temporary journaled sealing; inability to seal fails closed.
Outside-root hardlinks remain denied; use `git clone --no-hardlinks` for local
clones. Scoped localhost networking requires explicit Network capability.
An entire live home directory, physical power-loss recovery and hostile host
races remain unqualified. See [Windows boundary status](../native/windows/boundary/CURRENT_STATE.md).

## First run

Open a terminal in your project and run `latch`. Use `/setup` to select a provider,
credential source, and model. You supply your own model-provider credential.
Then run:

```sh
latch doctor
```

This read-only preflight checks the sandbox, Git, ripgrep, configuration, state
directory, profile, and credential without contacting the provider. It returns
0 when all checks pass, 1 for a runtime prerequisite failure, or 2 for a
configuration/CLI error. Resume saved work with `latch --resume --latest`.
See the [README](../README.md#get-started) for interactive and machine CLI usage.

## Build from source

Install stable Rust and a C toolchain. Linux also needs its runtime packages;
Debian / Ubuntu builders can install `build-essential`. Windows builders need
the **x64 MSVC C++ Build Tools and Windows SDK** in addition to Git and ripgrep.
These compiler requirements apply to source builds, not binary installs.

```sh
git clone https://github.com/JinShuo-Li/latch.git
cd latch
cargo install --path crates/latch-cli --locked
```

Cargo installs to its configured bin directory (normally `~/.cargo/bin`).
For a local release build without installing, run `cargo build --release --locked`;
the executable is in `target/release/`.

## Troubleshooting

- **Download returns 404 / latest cannot be found:** inspect the Releases page.
  The chosen tag must have both the platform asset and `SHA256SUMS`. Older
  releases may not use the new format. Pin a suitable release or build from source.
- **Checksum is missing or mismatched:** nothing is installed. Retry from the
  official release; report a persistent mismatch rather than bypassing validation.
- **`latch` is not found:** add the installation directory to PATH as described
  above. `latch --version` confirms which binary the shell resolves.
- **`GLIBC_2.xx not found`:** use a glibc 2.35+ distribution or compile from source
  on your host. The GNU archive is not compatible with Alpine's musl runtime.
- **Bubblewrap / namespaces unavailable:** run `latch doctor`; check your distro's
  user-namespace and AppArmor policy with your administrator. Latch refuses
  execution rather than weakening the sandbox.
- **Windows replacement fails:** close running Latch processes and check write
  permissions on the install directory, then rerun the installer.
- **Windows script execution is restricted:** the one-liner runs in the current
  PowerShell session. For downloaded files, inspect them and follow your
  organization's execution policy; no machine-wide policy change is required
  by Latch.
