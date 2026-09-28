# Environment for cross-compiling the Windows builds from macOS.
#
# Source it from the repository root:
#
#   . cross/env.sh x64      # x86_64-pc-windows-msvc (clang-cl + lld + xwin)
#   . cross/env.sh arm64    # aarch64-pc-windows-gnullvm (llvm-mingw)
#
# Toolchain locations default to Homebrew on Apple Silicon, ~/.xwin and
# ~/llvm-mingw; override LLVM_BIN, LLD, XWIN_DIR, XWIN_SDK_VERSION or
# LLVM_MINGW before sourcing if yours differ. Nothing here is read by a plain
# `cargo build`, so native builds and CI are unaffected.
#
# Also writes cross/local.ini (gitignored) with the machine-specific paths the
# Meson cross files use; pass it as the first --cross-file.

if [ ! -f cross/env.sh ]; then
  echo "cross/env.sh: source this from the repository root" >&2
  return 1 2>/dev/null || exit 1
fi

ZT_REPO="$(pwd)"
export LLVM_BIN="${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}"
export LLD="${LLD:-/opt/homebrew/opt/lld/bin/lld}"
export XWIN_DIR="${XWIN_DIR:-$HOME/.xwin}"
export XWIN_SDK_VERSION="${XWIN_SDK_VERSION:-10.0.26100}"
export LLVM_MINGW="${LLVM_MINGW:-$HOME/llvm-mingw}"

cat > cross/local.ini <<EOF
[constants]
repo = '$ZT_REPO'
llvm_bin = '$LLVM_BIN'
llvm_mingw_bin = '$LLVM_MINGW/bin'
EOF

case "$1" in
  x64)
    ZT_SDK_INC="$XWIN_DIR/sdk/include/$XWIN_SDK_VERSION"
    case ":$PATH:" in
      *":$ZT_REPO/cross/bin:"*) ;;
      *) export PATH="$ZT_REPO/cross/bin:$PATH" ;;
    esac
    export INCLUDE="$XWIN_DIR/crt/include;$ZT_SDK_INC/ucrt;$ZT_SDK_INC/um;$ZT_SDK_INC/shared"
    export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$ZT_REPO/cross/bin/lld-link"
    export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static"
    # For C dependencies built by cc-rs (e.g. ring).
    export CFLAGS_x86_64_pc_windows_msvc="-isystem$XWIN_DIR/crt/include -isystem$ZT_SDK_INC/ucrt -isystem$ZT_SDK_INC/um -isystem$ZT_SDK_INC/shared"
    export AR_x86_64_pc_windows_msvc="$ZT_REPO/cross/bin/llvm-lib"
    ;;
  arm64)
    export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER="$LLVM_MINGW/bin/aarch64-w64-mingw32-clang"
    # Link the C++ runtime statically so no libc++/libunwind DLLs are needed.
    export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-luuid -C link-arg=-Wl,-Bstatic -C link-arg=-lc++ -C link-arg=-lc++abi -C link-arg=-lunwind -C link-arg=-Wl,-Bdynamic"
    # For C dependencies built by cc-rs (e.g. ring).
    export CC_aarch64_pc_windows_gnullvm="$LLVM_MINGW/bin/aarch64-w64-mingw32-clang"
    export AR_aarch64_pc_windows_gnullvm="$LLVM_MINGW/bin/aarch64-w64-mingw32-ar"
    ;;
  *)
    echo "usage: . cross/env.sh x64|arm64" >&2
    return 1 2>/dev/null || exit 1
    ;;
esac
