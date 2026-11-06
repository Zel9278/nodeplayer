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
linker_var="CARGO_TARGET_${cargo_target}_LINKER"
cc=$(command -v "${!linker_var}")
cxx="${cc}++"
ar_var="AR_${env_target}"
ar=$(command -v "${!ar_var}")
ranlib="${ar%-ar}-ranlib"
test -x "$cxx"
test -x "$ranlib"

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
EOF

{
  echo "SDKROOT=$sdk"
  echo "CC_${env_target}=$cc"
  echo "CXX_${env_target}=$cxx"
  echo "RANLIB_${env_target}=$ranlib"
  echo "CMAKE_TOOLCHAIN_FILE_${env_target}=$toolchain"
  # setup-osxcross defaults to allowing undefined symbols. Require ordinary
  # framework/library resolution so missing dependencies fail during the build.
  echo "CARGO_TARGET_${cargo_target}_RUSTFLAGS="
} >> "$GITHUB_ENV"
