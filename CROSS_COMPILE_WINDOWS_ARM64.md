# Cross-compiling for Windows ARM64 on macOS

Builds a Windows ARM64 (`Aarch64`) GUI executable from an Apple Silicon host.

## Why not the MSVC target?

The `aarch64-pc-windows-msvc` route used for x64 (see
[CROSS_COMPILE_WINDOWS_MSVC.md](CROSS_COMPILE_WINDOWS_MSVC.md)) is not
possible here: `xwin` cannot fetch the ARM64 MSVC CRT (`libcmt`/`libvcruntime`
for ARM64 are not in the Microsoft manifest it uses), so `+crt-static`
linking cannot work.

Instead we target **`aarch64-pc-windows-gnullvm`** with the
[llvm-mingw](https://github.com/mstorsjo/llvm-mingw) toolchain. The result is
a real Windows ARM64 PE that links the UCRT dynamically (a standard OS
component on Windows 10+); the C++ runtime (libc++/libunwind) is linked
statically, so no extra DLLs need to be shipped.

## Prerequisites

```sh
rustup target add aarch64-pc-windows-gnullvm
```

Download and extract llvm-mingw (macOS universal build) to `~/llvm-mingw`:

```sh
curl -sL -o /tmp/llvm-mingw.tar.xz \
  "https://github.com/mstorsjo/llvm-mingw/releases/download/<release>/llvm-mingw-<release>-ucrt-macos-universal.tar.xz"
tar xf /tmp/llvm-mingw.tar.xz -C ~
mv ~/llvm-mingw-<release>-ucrt-macos-universal ~/llvm-mingw
```

Everything below assumes `~/llvm-mingw`; set `LLVM_MINGW` before sourcing
the environment script if yours is elsewhere. From the repository root:

```sh
. cross/env.sh arm64
```

This sets the cargo linker and link flags for `aarch64-pc-windows-gnullvm`
(`CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_*`), `CC_*`/`AR_*` for C
dependencies, and writes `cross/local.ini` (gitignored) with the toolchain
path the Meson cross file uses. The checked-in `.cargo/config.toml` is not
involved, so native builds and CI are unaffected.

## 1. Build libui-ng (Windows ARM64)

libui's Windows C++ uses an MSVC-ism (`&rt->VirtualMethod`) that clang in
GCC mode rejects. Build from a staged copy and extend the MSVC guard to also
cover clang:

```sh
rm -rf /tmp/ztarm && mkdir -p /tmp/ztarm
cp -R libui-ng /tmp/ztarm/libui-ng

# clang-incompatible COM member-function-pointer workarounds
cd /tmp/ztarm/libui-ng
python3 - <<'EOF'
import glob
for p in glob.glob('windows/*.cpp'):
    s = open(p).read()
    n = s.replace("#ifdef _MSC_VER", "#if defined(_MSC_VER) || defined(__clang__)")
    if n != s:
        open(p, 'w').write(n)
        print('patched', p)
EOF

rm -rf build   # the copy may contain a native build dir
meson setup build \
  --cross-file <repo>/cross/local.ini \
  --cross-file <repo>/cross/aarch64-windows-gnu.txt \
  --buildtype=release --default-library=static --backend=ninja
ninja -C build meson-out/libui.a

cp build/meson-out/libui.a <repo>/libui-ng/build/meson-out/libui.a
```

> The cross file `cross/aarch64-windows-gnu.txt` points at the `aarch64`
> clang/clang++/ar from `$LLVM_MINGW/bin` (via `cross/local.ini`) and sets
> `cpp_args = -fms-extensions`.

> Note: this overwrites the macOS `libui.a` in the shared `libui-ng/build`
> directory. It is a gitignored build artifact; a later `make` for macOS
> regenerates it.

## 2. Build the tray library (Windows ARM64)

```sh
cd <repo>/tray
rm -f libzt_desktop_tray.a
"$LLVM_MINGW/bin/aarch64-w64-mingw32-gcc" -O2 -DTRAY_WINAPI=1 -std=c99 -fno-exceptions \
  -c zt_desktop_tray.c -o /tmp/ztray_arm64.o
"$LLVM_MINGW/bin/aarch64-w64-mingw32-ar" rcs libzt_desktop_tray.a /tmp/ztray_arm64.o
```

> This also overwrites the gitignored macOS `libzt_desktop_tray.a`.

## 3. Keep the C++ runtime static

The exe must not depend on `libc++.dll`/`libunwind.dll`. libc++/libc++abi are
linked statically through the link flags set by `cross/env.sh`; for
libunwind the dynamic import lib is disabled so the `-lunwind` that rust's
`std` emits resolves to the static archive:

```sh
mv "$LLVM_MINGW/aarch64-w64-mingw32/lib/libunwind.dll.a" \
   "$LLVM_MINGW/aarch64-w64-mingw32/lib/libunwind.dll.a.disabled"
```

## 4. Build the Rust binary

`. cross/env.sh arm64` configures the target (`aarch64-pc-windows-gnullvm`):

* linker = `$LLVM_MINGW/bin/aarch64-w64-mingw32-clang`
* link args: `-fuse-ld=lld`, `-luuid`, and `-Wl,-Bstatic` around `-lc++`,
  `-lc++abi`, `-lunwind`
* `CC_aarch64_pc_windows_gnullvm` / `AR_aarch64_pc_windows_gnullvm` for C
  dependencies built by `cc-rs` (e.g. `ring`)

```sh
cargo build --target aarch64-pc-windows-gnullvm
# release:
# cargo build --release --target aarch64-pc-windows-gnullvm
```

If you changed the toolchain (e.g. the libunwind rename) but cargo reports the
build is already up to date, force a relink with `touch src/main.rs`.

Output: `target/aarch64-pc-windows-gnullvm/debug/zerotier_desktop_ui.exe`

## Verification

```sh
file target/aarch64-pc-windows-gnullvm/debug/zerotier_desktop_ui.exe
# PE32+ executable (GUI) Aarch64, for MS Windows

# Only standard Windows system DLLs should remain (no libc++/libunwind):
llvm-objdump -p target/aarch64-pc-windows-gnullvm/debug/zerotier_desktop_ui.exe | rg "DLL Name" | sort -u
```

## Caveats

* Flavor is GNU/LLVM-MinGW (`aarch64-pc-windows-gnullvm`), not the MSVC build
  CI produces for x64/x86.
* The UCRT is linked dynamically (`api-ms-win-crt-*.dll`), which is part of
  Windows 10+.
* The two one-line libui patches exist only in the staged copy under `/tmp`,
  not in the vendored `libui-ng/` sources.
* Only `debug` is covered above; `--release` needs the same environment.
