# Experimental remote plugin backend

This module owns VST3 / CLAP **whole-plugin child processes**, including component,
controller, native editor, state and destruction. It is not an OS permission sandbox.
It must be explicitly enabled by the parent; it never falls back to loading a binary
in the parent. JSFX / Native are not routed here.

## Integration

- `create(&PluginInfo, sample_rate: f32, max_block: usize) -> Result<Created>` returns
  a pending proxy immediately. No file/mmap creation, process spawn, plugin load or
  synchronous RPC is performed on this calling thread.
- `worker_main(&[OsString]) -> Result<()>` consumes only arguments **after**
  `--plugin-worker`: loopback socket address, random token, mapping path, generation.
  Run this entry before ordinary GUI, audio device, extensions or session startup.
- Worker uses `Host::default().create_local`, avoiding recursive isolation.
- Optional instance hooks: `remote_status`, `request_remote_restart`, `bridge_latency`,
  `remote_counters` (underrun blocks, overflow events/slot submissions).
- `idle` adopts metadata; `wants_restart` signals port/latency/parameter topology
  changes. `restart` returns a new *proxy*, not a new child.
- `Editor::ready` is a cached acknowledgement that child open completed, not merely
  that parent enqueued a request. Each editor has a monotonic token; acknowledgements,
  geometry and close requests cannot accidentally affect a replacement editor.

## Data plane and timeline

Native 64-bit targets with 32/64-bit atomics only. The file-backed memmap contains
no process addresses, Rust containers, rtrb endpoints or ownership pointers. Header
checks validate protocol version, layout size, immutable worker generation, quantum
and capacities. An independently advanced processing epoch invalidates samples on
state changes, checkpoint quiescence and local plugin restart.

Current **provisional**, not performance-qualified values:

| Setting | Value |
|---|---:|
| Child quantum Q | 256 frames |
| Bridge delay D | 4096 frames (16 Q) |
| Slots | 32 |
| Short MIDI per slot | 512 |
| Parent tagged audio history | 131072 frames |
| Maximum plugin algorithm latency | 65536 frames |
| State cache blob | 1 MiB |
| Parameter count | 8192 |
| Control frame | 8 MiB |
| Parent control queue | 128 commands |

The adapter collects Q from arbitrary callback lengths. Output from input sample S
is delivered only at S+D; plugin algorithm latency is reported separately and is
already present in plugin output. Results are tagged by absolute local sample
cursor, epoch and sequence. A late result is discarded rather than replayed at a
later callback. MIDI offsets are translated through the same S+D timeline. Equal
sample MIDI order uses source sequence and event index. Input must be time-sorted
(as guaranteed by the engine MidiBuf). Future-out-of-current-block input MIDI is
not clamped into the present.

Slot ownership is `FREE -> INPUT -> WORKING -> OUTPUT -> FREE`, with release
publication/acquire consumption. Only parent writes FREE and reads OUTPUT; only
child reads INPUT and writes WORKING. An obsolete epoch INPUT may be discarded by
the child; a WORKING slot is never reclaimed by the parent. Child selects the
minimum pending sequence (not physical array index). The parent never waits for
completion and never spins for a free slot.

Every output sample is overwritten. Missing results use fixed D+algorithm-latency
dry history for effects and silence for instruments. MIDI is **not** replayed on
fault; bounded all-notes-off/sustain-reset is emitted after detected discontinuity.
Overflow and underrun counters are available; no malloc, formatting, file IO,
spawn, join or blocking lock is used by parent process callbacks. Parent uses
`try_lock` only to adopt an already-created mapping. Tagged histories and future
MIDI storage are allocated before the processor reaches audio.

D is not a hard realtime guarantee. No maximum continuous callback burst H has
been measured or enforced; a burst beyond available worker runway produces
fixed-timeline fallback. No claim of zero underruns, seamless recovery or bounded
real-device latency is made. Per-generation priming suppresses startup underrun
accounting, not the initial D silence/dry-fill itself.

## Control, authentication and watchdogs

Control is dedicated loopback TCP with 256-bit `ring::rand` credentials, fixed
protocol version/generation, strictly increasing request ID, bounded JSON frame
size, absolute whole-frame read/write deadlines and no external-interface bind.
This fallback avoids adding dependencies. Token is compared without content-
dependent early exit. Plugin stdout/stderr are null, not IPC pipes, so plugin log
flooding cannot deadlock or fill disk. Child args contain token and mapping path:
a same-user privileged process may inspect them. Windows inherited temp directory
ACLs are relied on; Unix mapping files are mode 0600. This is crash isolation, not
isolation from a malicious same-user process. Owner-only named pipes/UDS would be
a preferable future hardening step.

Parent supervisor sends polling requests independently of UI (typically 8 ms when
idle), detects child exit, control timeout (3 s), startup timeout (30 s), and DSP
progress timeout (2 s while parent cursor continues advancing). A single blocked
control RPC can delay the DSP-progress check up to its own bounded 3 s deadline.
Unauthenticated connection hello is limited to 250 ms per attempt and the overall
startup deadline. No stdout protocol is used.

Child independently exits if parent control heartbeat ceases for 20 s, with a
35 s startup allowance; this watchdog starts before plugin creation. Windows
supervisor also attempts a kill-on-job-close Job Object (nested-job/policy failures
fall back to TCP heartbeat); child spawn-to-job assignment has an unavoidable
small race because std::process does not expose suspended spawn here. Parent
supervisor kills and reaps on all session exits. Parent UI/RT Drop never waits.
Unix/macOS-specific parent-death primitives are not installed.

## Ownership and state

RemoteInstance alone owns worker stop intent. Deactivating or dropping an old
RemoteProcessor during graph port/latency replacement does **not** stop a child.
The supervisor remains the final Core owner until instance, editor and processor
proxies retire. Retired generations are reclaimed on the supervisor thread once
no processor references remain. Shared Drop unmaps before unlink (important on
Windows). This favors safety over early cleanup if the engine leaks a proxy.

Child DSP thread alone calls old Processor::deactivate. State load/save and plugin
restart park DSP, invalidate the epoch, invoke main-thread instance operation,
and send the replacement processor back to the same child audio thread. A
child-only join guard ensures DSP exits before plugin Instance/module teardown;
if a plugin hangs here parent supervisor kills/reaps it. Before checkpoint or
plugin-requested restart, the child processes one silent Q-frame quantum and
discards its outputs to drain queued parameter writes. This advances internal
plugin state by Q and is not a transparent/seamless snapshot. State load skips
that drain because the incoming state supersedes preceding parameter writes.

Parent parameter/state/editor operations only use caches and bounded enqueue.
Queue-full `load_state` returns an error; parameter/editor writes may be dropped
and retried by normal UI updates. Supervisor coalesces geometry/zoom and repeated
parameter/display writes within bounded batches. Save returns last cached state
and requests a new checkpoint; initial cache may be absent. Requested load blobs
are preserved through faults. Automatic checkpoint interval is 30 s; checkpoint
itself quiesces/restarts child DSP and may audibly interrupt processing. Parameter
journal is replayed after cached state on manual restart. A requested state load
clears preceding parameter writes; an acknowledged checkpoint clears the journal
only when it supplies a new cached state. Coalescing never crosses state/editor
lifecycle barriers. This is still not a transactional state/parameter version history.
The known diagnostic fixture's fault trigger params >=100 are not replayed on
manual restart; production plugins have no such special handling.

No infinite automatic restart loop: recovery is explicit. A changed sample rate
requires creating a new remote instance; metadata-only proxy restart does not
reconfigure worker sample rate. Full editor focus/IME/DPI and real-device tests
are required before considering this backend production-ready.

## Editors and supported displays

Parent RemoteEditor performs **no native window operations**. It queues Open,
Place(Spot), Zoom and Close; child opens existing Instance editor at Embedded
parent HWND/XID or Window and owns every resize/reparent/clip/native call.
Windows child pumps PeekMessageW/TranslateMessage/DispatchMessageW in bounded
batches and initializes COM apartment. Linux relies on existing editor idle/run
loop behavior. macOS and displays with no X11 DISPLAY (native Wayland) are rejected
before isolation startup. XWayland is allowed but unverified.

Cross-process HWND embedding can still induce OS-level synchronous interactions
in the parent toolkit. Avoiding native calls in RemoteEditor is necessary but
**does not prove GUI-hang isolation**. Required embed/zoom/clip/DPI/focus/modal/GUI
hang tests must be measured. macOS remote NSView is not implemented.

## Tests in this module

- A second real mmap validates slot publication and generation checks.
- An oversized control prefix is rejected before body allocation.
- A deterministic in-memory echo consumer exercises irregular 1/127/256/511/512/
  1024 callbacks, exact D audio/MIDI arrival, late result rejection, full slots,
  busy-slot ownership, full-output overwrite and processing-epoch invalidation.

These tests do not substitute for actual worker process, real CLAP/VST3 editor,
state-hang, parent-kill orphan, platform GUI or real-device deadline measurements.
