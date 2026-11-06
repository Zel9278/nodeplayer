#!/usr/bin/env bash
# Run after mbround18/setup-osxcross. Export target-specific tools for cc-rs/CMake,
# keeping build scripts and proc macros on the Linux host compiler.
set -euo pipefail

target=${1:?Rust target is required}
case "$target" in
  aarch64-apple-darwin) arch=arm64 ;;
  x86_64-apple-darwin) arch=x86_64 ;;
  *) echo "Unsupported osxcross target: $target" >&2; exit 1 ;;
esac
: "${OSXCROSS_TARGET:?setup-osxcross must run first}"
: "${GITHUB_ENV:?GitHub Actions environment file is required}"
sdk="$OSXCROSS_TARGET/SDK/MacOSX14.5.sdk"
test -d "$sdk"
env_target=${target//-/_}
cargo_target=${env_target^^}
ar_var="AR_${env_target}"
ar=$(command -v "${!ar_var}")
prefix=${ar%-ar}
# The action can pick a *-cmake-clang helper from its filename glob. Derive
# exact tool names from ar instead; the helper has no matching clang++ binary.
cc="${prefix}-clang"
cxx="${prefix}-clang++"
ranlib="${prefix}-ranlib"
ld="${prefix}-ld"
test -x "$cc"
test -x "$cxx"
test -x "$ranlib"
test -x "$ld"

mkdir -p target/osxcross
toolchain="$PWD/target/osxcross/$target.cmake"
cat > "$toolchain" <<EOF
set(CMAKE_SYSTEM_NAME Darwin)
set(CMAKE_SYSTEM_PROCESSOR $arch)
set(CMAKE_OSX_ARCHITECTURES $arch CACHE STRING "")
set(CMAKE_OSX_SYSROOT "$sdk" CACHE PATH "")
set(CMAKE_OSX_DEPLOYMENT_TARGET 11.0 CACHE STRING "")
set(CMAKE_C_COMPILER "$cc")
set(CMAKE_CXX_COMPILER "$cxx")
set(CMAKE_AR "$ar")
set(CMAKE_RANLIB "$ranlib")
# cc-rs supplies --target=*-apple-macosx. Clang may then miss the Darwin-prefixed
# linker and fall back to /usr/bin/ld, even with the osxcross compiler wrapper.
set(CMAKE_EXE_LINKER_FLAGS_INIT "-fuse-ld=$ld")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "-fuse-ld=$ld")
set(CMAKE_MODULE_LINKER_FLAGS_INIT "-fuse-ld=$ld")
EOF

{
  echo "SDKROOT=$sdk"
  echo "CC_${env_target}=$cc"
  echo "CXX_${env_target}=$cxx"
  echo "RANLIB_${env_target}=$ranlib"
  echo "CMAKE_TOOLCHAIN_FILE_${env_target}=$toolchain"
  echo "CARGO_TARGET_${cargo_target}_LINKER=$cc"
  # setup-osxcross defaults to allowing undefined symbols. Require ordinary
  # framework/library resolution so missing dependencies fail during the build.
  echo "CARGO_TARGET_${cargo_target}_RUSTFLAGS=-C link-arg=-fuse-ld=$ld"
} >> "$GITHUB_ENV"

# Exercise the same cc-rs target spelling before starting the full Rust build.
# Neither check executes the Mach-O output on Linux.
printf 'int main(void) { return 0; }\n' |
  "$cc" --target="$arch-apple-macosx" "-fuse-ld=$ld" -x c - \
    -o "target/osxcross/$target-smoke-c"
printf '#include <vector>\nint main() { return std::vector<int>{1}.front() - 1; }\n' |
  "$cxx" --target="$arch-apple-macosx" "-fuse-ld=$ld" -std=c++17 -x c++ - \
    -o "target/osxcross/$target-smoke-cxx"
