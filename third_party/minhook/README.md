# Nodeplayer's MinHook copy

This directory contains the build sources and tests from the `minhook` 0.9.0
crate, including its bundled MinHook C library. The Rust wrapper retains its MIT
license. The C library and disassembler notices are in
[`minhook/LICENSE.txt`](minhook/LICENSE.txt).

Nodeplayer changes thread enumeration in `minhook/src/hook.c`:

- Try a Windows process snapshot with `PSS_CAPTURE_THREADS` first. Read only the
  current process's thread IDs and exclude the installing thread.
- Retain the original Toolhelp enumeration if capturing or walking the process
  snapshot fails. Discard any partial list before falling back.
- Keep MinHook's existing thread suspension, instruction-pointer relocation,
  patching, and resumption code.

The original implementation walks every thread on the system before filtering
for the current process. On a machine with many unrelated threads, that made
first editor opening take seconds even when hook activation was batched.
`build.rs` selects Windows 8.1 APIs for the C source. Process snapshot
failures retain the original enumeration path.

API references: [PssCaptureSnapshot](https://learn.microsoft.com/en-us/windows/win32/api/processsnapshot/nf-processsnapshot-psscapturesnapshot)
and [PssWalkSnapshot](https://learn.microsoft.com/en-us/windows/win32/api/processsnapshot/nf-processsnapshot-psswalksnapshot).

The native regression in `tests/thread_enumeration.c` checks that all live
helper threads are included, the installing thread is excluded, IDs belong to
this process and do not repeat, and fallback still works after snapshot failure,
a partial walk, or an invalid empty thread count. From an x64 Native Tools
Command Prompt in this directory:

```bat
cl /nologo /MT /D_WIN32_WINNT=0x0603 /DMINHOOK_THREAD_ENUMERATION_MAIN /Fethread_enumeration.exe tests/thread_enumeration.c minhook/src/buffer.c minhook/src/trampoline.c minhook/src/hde/hde64.c
thread_enumeration.exe
```

The upstream crate README follows.

---

# minhook

[![CI](https://github.com/Jakobzs/minhook/actions/workflows/test.yml/badge.svg)](https://github.com/Jakobzs/minhook/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/minhook)](https://crates.io/crates/minhook)
[![rustdoc](https://img.shields.io/badge/docs-rustdoc-brightgreen)](https://jakobzs.github.io/minhook/minhook)
[![codecov](https://codecov.io/github/Jakobzs/minhook/graph/badge.svg?token=PGVBSDVD83)](https://codecov.io/github/Jakobzs/minhook)
![MSRV](https://img.shields.io/badge/rust-1.85+-brightgreen.svg?&logo=rust)

A Rust wrapper for the [MinHook](https://github.com/TsudaKageyu/minhook) library.

## Usage

Add `minhook` by using the following command:

```bash
cargo add minhook
```

Or if you prefer to add it manually, you can do so by editing your `Cargo.toml` file:

```toml
[dependencies]
minhook = "0.9.0"
```

## Example

This example shows how to create a hook for a function, and also call the original function.

```rust
use minhook::{MinHook, MH_STATUS};

fn main() -> Result<(), MH_STATUS> {
    // Create a hook for the return_0 function, detouring it to return_1
    let return_0_address = unsafe { MinHook::create_hook(return_0 as _, return_1 as _)? };

    // Enable the hook
    unsafe { MinHook::enable_all_hooks()? };

    // Call the detoured return_0 function, it should return 1
    assert_eq!(return_0(), 1);

    // Transmute the original return_0 function address to a function pointer
    let return_0_original = unsafe { std::mem::transmute::<_, fn() -> i32>(return_0_address) };

    // Call the original return_0 function
    assert_eq!(return_0_original(), 0);

    Ok(())
}

fn return_0() -> i32 {
    0
}

fn return_1() -> i32 {
    1
}
```

## License

This project is licensed under the MIT license ([LICENSE](LICENSE) or http://opensource.org/licenses/MIT).

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the MIT license, shall be licensed as above, without any additional terms or conditions.
