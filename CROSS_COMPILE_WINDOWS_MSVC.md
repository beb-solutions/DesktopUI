# Cross-compiling for Windows x64 (MSVC) on macOS

Builds the same `x86_64-pc-windows-msvc` flavor that CI produces, but from a
macOS (Apple Silicon) host. The result is a statically linked
(`+crt-static`) Windows GUI executable with no MSVC/MinGW runtime DLL
dependency.

> Note: this documents the toolchain and the pitfalls we hit. All paths are
> machine-specific (`/opt/homebrew`, `~/.xwin`, repo location) and must be
> adjusted if your layout differs. Only `x86_64` is covered; `i686` (32-bit)
> is out of scope here.

## Prerequisites / installation

```sh
rustup target add x86_64-pc-windows-msvc
brew install mingw-w64 lld xwin     # xwin = MS CRT + Windows SDK downloader

# Download the MS CRT and Windows SDK (asks to accept the license)
printf 'yes\n' | xwin splat --output ~/.xwin
```

`xwin` produces (checked on this machine):

| purpose            | path                                     |
| ------------------ | ---------------------------------------- |
| CRT headers        | `~/.xwin/crt/include`                    |
| CRT libs (x64)     | `~/.xwin/crt/lib/x86_64`                 |
| SDK headers        | `~/.xwin/sdk/include/10.0.26100/{ucrt,um,shared}` |
| SDK libs (x64)     | `~/.xwin/sdk/lib/{ucrt,um}/x86_64`       |

Two layout notes:

* `xwin splat` also drops an unpack cache into `./.xwin-cache` when run from
  the repo root (gitignored).
* On a case-insensitive filesystem `xwin` disables the casing symlinks it
  normally creates; that is fine on APFS.

## Helper scripts (`cross/`)

These wrappers encode the workarounds below and are referenced from the Meson
cross file / cargo config:

* `cross/bin/clang-cl` – C++ compiler wrapper. Meson forces `cpp_std=c++11`,
  but the current MSVC STL needs C++14+. The wrapper drops the injected
  `-std=c++11` and enforces `/clang:-std=c++17`. It must be *named*
  `clang-cl` so Meson still treats it as the clang-cl driver.
* `cross/bin/llvm-lib` – archiver wrapper adding `/MACHINE:X64` (Meson would
  otherwise pass the host machine type, `arm`).
* `cross/bin/lld-link` – linker wrapper. rustc drives the MSVC link through
  the multi-call `lld` (`-flavor link`); the wrapper appends the xwin
  `/libpath` directories plus the default Windows libraries that `link.exe`
  would add automatically but `lld` does not.
* `cross/x86_64-windows-msvc-clang.txt` – Meson cross file.

The wrappers contain absolute paths to `/opt/homebrew` (Homebrew on Apple
Silicon) and `~/.xwin`; adjust if needed.

## 1. Build libui-ng (Windows, static)

Stage libui-ng under `/tmp` before compiling: `clang-cl` misparses Unix
absolute paths that start with an option letter (e.g. `/Users/...` begins
with `/U`), so a repo under `/Users` must not be used as the source/build
directory for clang-cl.

```sh
rm -rf /tmp/ztwin && mkdir -p /tmp/ztwin
cp -R libui-ng /tmp/ztwin/libui-ng

export XW=~/.xwin
export INCLUDE="$XW/crt/include;$XW/sdk/include/10.0.26100/ucrt;$XW/sdk/include/10.0.26100/um;$XW/sdk/include/10.0.26100/shared"
export PATH="$PWD/cross/bin:$PATH"

cd /tmp/ztwin/libui-ng
meson setup build-win \
  --cross-file <repo>/cross/x86_64-windows-msvc-clang.txt \
  --buildtype=release -Db_vscrt=mt --default-library=static --backend=ninja
ninja -C build-win meson-out/libui.a
```

Copy the result into the directory `build.rs` looks at, under the name cargo
expects for the MSVC target:

```sh
cp build-win/meson-out/libui.a <repo>/libui-ng/build/meson-out/ui.lib
```

## 2. Build the tray library (Windows) with MinGW

Same recipe as CI (`TRAY_WINAPI=1`, `-m64`):

```sh
cd <repo>/tray
x86_64-w64-mingw32-gcc -Og -DTRAY_WINAPI=1 -std=c99 -m64 -fno-exceptions \
  -static -c zt_desktop_tray.c -o /tmp/zt_desktop_tray_win.o
x86_64-w64-mingw32-ar rcs <repo>/tray/zt_desktop_tray.lib /tmp/zt_desktop_tray_win.o
```

## 3. Build the Rust binary

`.cargo/config.toml` already pins the target to the `lld-link` wrapper with
`+crt-static`. The extra env below is only needed for C dependencies built by
`cc-rs` (e.g. `ring`): the host `cc`/`clang` is used to compile them for the
Windows target, so it needs the SDK headers and an MSVC-style archiver.

```sh
export PATH="<repo>/cross/bin:$PATH"
export CFLAGS_x86_64_pc_windows_msvc="\
  -isystem$XW/crt/include \
  -isystem$XW/sdk/include/10.0.26100/ucrt \
  -isystem$XW/sdk/include/10.0.26100/um \
  -isystem$XW/sdk/include/10.0.26100/shared"
export AR_x86_64_pc_windows_msvc="<repo>/cross/bin/llvm-lib"

cargo build --target x86_64-pc-windows-msvc
# release:
# cargo build --release --target x86_64-pc-windows-msvc
```

Output: `target/x86_64-pc-windows-msvc/debug/zerotier_desktop_ui.exe`
(release dir for `--release`).

## Verification

```sh
file target/x86_64-pc-windows-msvc/debug/zerotier_desktop_ui.exe
# PE32+ executable (GUI) x86-64, for MS Windows

# Only standard Windows system DLLs should be imported (no MSVC/MinGW runtime):
llvm-objdump -p target/x86_64-pc-windows-msvc/debug/zerotier_desktop_ui.exe | rg "DLL Name" | sort -u
```

## Pitfalls (why the wrappers exist)

* `clang-cl` on a Unix host treats `/Users/...` as the `/U` option; compile
  from a path that does not collide (e.g. `/tmp`).
* Meson forces `cpp_std=c++11` (libui validates the option) while the current
  MSVC STL requires C++14+; Meson also strips any std override from
  `cpp_args`. The `clang-cl` wrapper strips/re-injects the standard.
* Meson derives the archiver machine type from the *build* host; `llvm-lib`
  then gets `/MACHINE:arm`. The `llvm-lib` wrapper forces `/MACHINE:X64`.
* rustc invokes the MSVC linker as `lld -flavor link`, which the flavor-locked
  `lld-link` binary does not accept. The wrapper calls the multi-call `lld`.
* `lld` does not emulate `link.exe`'s default-library list, so the wrapper
  appends the standard Windows import libs (kernel32, user32, gdi32, ...).
