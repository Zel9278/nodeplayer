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
  `--plugin-worker`: loopback socket address, mapping path, generation; the 256-bit token is
  sent as the first stdin line, never in argv.
  Run this entry before ordinary GUI, audio device, extensions or session startup.
- Worker uses `Host::default().create_local`, avoiding recursive isolation.
- Optional instance hooks: `remote_status`, `request_remote_restart`, `bridge_latency`,
  `remote_counters` (underrun blocks, overflow events/slot submissions).
- `idle` adopts latency, parameter and MIDI-port metadata without replacing the live
  processor. Graph routing and prepared dry-path timing update independently.
- `Editor::ready` is a cached acknowledgement that child open completed, not merely
  that parent enqueued a request. Each editor has a monotonic token; acknowledgements,
  geometry and close requests cannot accidentally affect a replacement editor.

## Data plane and timeline

Native 64-bit targets with 32/64-bit atomics only. The file-backed memmap contains
no process addresses, Rust containers, rtrb endpoints or ownership pointers. Header
checks validate protocol version, layout size, immutable worker generation, quantum
and capacities. An independently advanced processing epoch invalidates samples on
destructive state load. Warm plugin activation preserves the epoch and queued audio;
manual worker recreation supplies a new mapping generation.

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
| Captured editor image | 8192 per dimension, 16 Mi pixels / 64 MiB RGBA maximum |

The adapter collects Q from arbitrary callback lengths. Output from input sample S
is delivered only at S+D; plugin algorithm latency is reported separately and is
already present in plugin output. Results are tagged by absolute local sample
cursor, epoch and sequence. A late result is discarded rather than replayed at a
later callback. MIDI offsets are translated through the same S+D timeline. Equal
sample MIDI order uses source sequence and event index. Input must be time-sorted
(as guaranteed by the engine MidiBuf). Future-out-of-current-block input MIDI is
not clamped into the present.

Each input quantum includes the player's transport state and song position at its first
sample. A partial quantum retains that snapshot across parent callbacks; subsequent
quanta advance song time only while playing. The child applies the snapshot with the
audio, independent of when the worker catches up. Engine sample time continues through
pause/stop and remains separate from song position. Transport changes take effect at
child quantum boundaries, at most 256 input frames after a parent block boundary.

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
flooding cannot deadlock or fill disk. The token is passed through the child's stdin (first line), not argv; the mmap filename
contains only process/generation identifiers and is created privately (Unix mode 0600).
The child opens the mapping before connecting; after the authenticated hello and parent
init/reply handshake, the parent unlinks the pathname while mapped handles remain valid.
This is crash isolation, not isolation from a malicious same-user process. Owner-only named pipes/UDS would be
a preferable future hardening step.

Parent supervisor sends polling requests independently of UI (typically 8 ms when
idle), detects child exit, control timeout (3 s), startup timeout (30 s), and DSP
progress timeout (2 s while parent cursor continues advancing). A single blocked
control RPC can delay the DSP-progress check up to its own bounded 3 s deadline.
Each unauthenticated connection gets at most a 1 s authentication read deadline, bounded
independently by the overall 30 s startup deadline. No stdout protocol is used.

Child independently exits if parent control heartbeat ceases for 20 s, with a
35 s startup allowance; this watchdog starts before plugin creation. Windows
supervisor also attempts a kill-on-job-close Job Object (nested-job/policy failures
fall back to TCP heartbeat); child spawn-to-job assignment has an unavoidable
small race because std::process does not expose suspended spawn here. Parent
supervisor kills and reaps on all session exits. Parent UI/RT Drop never waits.
Unix/macOS-specific parent-death primitives are not installed.

## Ownership and state

RemoteInstance alone owns worker stop intent. Deactivating or dropping an old
RemoteProcessor does **not** stop a child.
The supervisor remains the final Core owner until instance, editor and processor
proxies retire. Retired generations are reclaimed on the supervisor thread once
no processor references remain. Shared Drop unmaps before unlink (important on
Windows). This favors safety over early cleanup if the engine leaks a proxy.

Child DSP thread alone calls old Processor::deactivate. State load and plugin-requested
restart park DSP, invoke the main-thread instance operation, and send the replacement
processor back to the same child audio thread. State load invalidates the processing
epoch; a warm restart retains real queued input and the parent dry/wet histories.
A child-only join guard ensures DSP exits before plugin Instance/module teardown; if a
plugin hangs here the parent supervisor kills/reaps it. Before plugin-requested restart,
pending VST3 and CLAP parameter changes flush on the audio thread without synthesizing
input or advancing the audio clock. VST3 uses zero-sample parameter-only processing;
CLAP uses its parameter flush extension. State load skips that flush.
Checkpoints save the live instance on the worker's main thread while DSP and parent audio
publication continue. Only destructive state loads quiesce the bridge during control RPC.

Local plugin restarts also retain the engine slot and processor wrapper while replacing
only its suspended inner processor. The delayed dry path keeps 131072 frames of real
input history, or the initial requested delay if larger, within a shared 128 MiB budget
for live, queued and retired plugin bypass storage. A latency increase beyond the retained
history needs new input to fill that additional span. Graph delay-plan changes transfer
prepared history pages on the audio thread and retire unused storage on the control thread.
Replacement latency and bypass storage must be admitted before the inner processor resumes.
Rejected restarts retain the suspended fallback and its last admitted graph timing.
Engine shutdown waits for an explicit audio-thread completion before plugin state save
and teardown; a bounded shutdown timeout retains in-process plugin owners until exit.

Parent parameter/editor operations use caches and bounded enqueue. Queue-full
`load_state` returns an error; parameter/native-placement writes may be dropped and
retried by normal UI updates. Captured button transitions have a separate bounded
retry queue. Supervisor coalescing never crosses state/editor lifecycle barriers.
Explicit saves wait up to 3 s for a checkpoint sequence acknowledgement, including an
unchanged state blob; timeout falls back to the cache, which may initially be absent.
Session autosave requests all isolated checkpoints together and polls their acknowledgements
on subsequent UI ticks. After completion, or the same 3 s deadline, it serializes the latest
completed snapshots without waiting on the UI thread. Preset saves and shutdown still use
fresh checkpoints. Local-format state APIs remain on their owning thread.
Requested load blobs are preserved through faults. Automatic checkpoint interval is
30 s. Parameter journal is replayed after cached state on manual restart; a requested
load or acknowledged state checkpoint clears the preceding parameter overlay. This is
still not a transactional state/parameter version history.
The known diagnostic fixture's fault trigger params >=100 are not replayed on
manual restart. This deterministic fixture-only behavior is enabled by the parent's
`NODEPLAYER_DIAGNOSTIC_WORKER=1` marker (diagnostic scenario runs only); production
workers do not receive this special journal handling.

No infinite automatic restart loop: recovery is explicit. A changed sample rate
requires creating a new remote instance; metadata-only proxy restart does not
reconfigure worker sample rate. Full editor focus/IME/DPI and real-device tests
are required before considering this backend production-ready.

## Editors and supported displays

Parent RemoteEditor performs **no native window operations**. Every embedded editor
request creates its source in a private native host in the worker, without a second
desktop window. Popped-out editors use native top-level windows.
AppKit/Win32 printing/XComposite capture and native pointer delivery stay on the worker
main thread. The parent paints the image in the ordinary
graph layer, checks coverage after all nodes/layers are known, and inverse-maps input.
Graph movement and zoom transform the latest image on every parent frame, including
frames without a new capture. They do not move, mask, resize, or rescale the native source.
The editor API and worker protocol carry no graph-scale requests.
Pop-out editors retain ordinary native behavior.
Windows layered popup windows require compositor bitmap capture, which this Win32
printing backend does not provide. They produce an explicit capture error directing
the user to Pop out; a stock-window caption is not accepted as the popup image.

Captured RGBA8 pixels use a separate bounded latest-frame mmap; they never enter the
audio slots or the JSON control replies. Atomic slot ownership allows replacing an
unread frame and prevents overwriting one the parent is copying. Header/format,
dimensions, byte count, frame sequence, worker generation, and editor token are
validated before pixels become an egui texture. A retired surface cannot feed a
replacement editor. Supervisor creates/maps storage off the UI thread, and both
peers retry file cleanup after unmapping. The supervisor also retains cleanup paths
until the worker is reaped, covering forced termination and failed surface setup.
On macOS, up to 16 attached child popup windows share the root image's publication
lease and total pixel budget. The parent draws them above the graph, preserves their
native input coordinates through zoom and viewport clamping, and dismisses interactive
popups on Escape, outside clicks, focus loss, or hiding the graph.

Pointer messages carry the same worker generation and editor token, plus the modifier
state recorded before queueing (including Shift for fine drags). Adjacent moves
may coalesce; button and wheel barriers preserve order. Click counts are determined
in the parent before worker queueing, so control delays cannot turn double-clicks
into single clicks or combine separate clicks. macOS and Windows use their system
double-click settings. Transitions retry through queue pressure without blocking
the parent. If even the bounded transition queue
fills, a reserved close slot closes the editor rather than leaving a stuck drag.
Keyboard/IME, drag-and-drop, blocking native menus, and unattached popup windows are
not bridged. Attached child popup capture is supported on macOS; Windows also captures
owned native popup windows. The source
stays alive until the graph editor closes. Windows and X11 capture hosts remain mapped
outside the desktop; the AppKit host and its child popups are ordered transparently
without activating the source host. Plugins can observe a visible parent without
showing a second desktop window. GPU editor capture
remains renderer-dependent.
On macOS, a full-editor ordinary `CAMetalLayer` using BGRA8/RGBA8 can be read back from
completed drawables. The adapter applies only to that source layer, keeps at most one
readback in flight, and retires its native registration before plugin GUI teardown.
Mixed GPU/CPU view layouts and custom Metal-layer subclasses still need another adapter.
Capture service targets 240 updates per second and skips missed deadlines after a stall.
The worker services native UI/capture between control requests without an 8 ms blocking
readiness poll. Parent repaint requests use the same target while a captured editor is
visible. CPU readback/upload, plugin rendering, viewport scheduling and the display can
each limit the measured rate; capture publication and parent delivery do not establish
compositor presentation FPS. A blank startup that produces no usable image within five
seconds exposes a capture error.
Each editor open retains its first capture failure, bounded to 1024 characters, separately
from the recoverable UI message. Worker metadata carries this history so diagnostic health
cannot miss a failure that recovers between polls. Reopening starts fresh capture history.

Windows pumps bounded native message batches and initializes COM. macOS requires
captured mode for isolation, initializes AppKit in the worker, and pumps events even
when no control request is pending; startup/teardown and every worker iteration use
autorelease pools. No parent NSView pointer crosses the boundary.
Linux relies on existing editor idle/run-loop behavior and requires X11/XWayland.
Native Wayland is unsupported. Plugin modules remain mapped until process exit so
deferred GUI cleanup can finish after editor destruction.

## Tests in this module

- A second real mmap validates slot publication and generation checks.
- An oversized control prefix is rejected before body allocation.
- Image transport checks malformed headers, bounds, stale sequences, concurrent copies,
  unread-frame replacement, cleanup, and publication from a real child process.
- Captured input tests cover queue pressure, transition order, reserved close, stale
  generations/tokens, and native placement/scale suppression.
- A deterministic in-memory echo consumer exercises irregular 1/127/256/511/512/
  1024 callbacks, exact D audio/MIDI arrival, late result rejection, full slots,
  busy-slot ownership, full-output overwrite and processing-epoch invalidation.

These tests do not substitute for actual worker process, real CLAP/VST3 editor,
state-hang, parent-kill orphan, platform GUI or real-device deadline measurements.
