//! Explicit-fixture, real-child headless bridge integration checks.
//!
//! No audio device, native editor, plugin installation, discovery, normal profile
//! restore, or synthetic/fake worker is involved. Run from the normal nodeplayer
//! executable so current_exe() can dispatch its real --plugin-worker child mode.
//! A caller-owned external watchdog is still required for process-wide failures.
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::{
    engine::{MAX_BLOCK, MidiBuf, MidiEvent, Processor, StereoBuf},
    plugins::{Format, Host, Instance, remote::BRIDGE_LATENCY},
};

const FIXTURE_ID: &str = "org.nodeplayer.test-fixture";
const RATE: f64 = 48_000.0;
const BLOCKS: [usize; 4] = [127, 256, 512, 1024];
const SAMPLE_EPSILON: f32 = 1.0e-6;

#[derive(Serialize)]
struct Observation {
    case: &'static str,
    passed: bool,
    gain_native: f32,
    algorithm_latency_frames: u32,
    bridge_latency_frames: u32,
    first_sample: u64,
    tested_frames: u64,
    verified_audio_impulses: usize,
    verified_midi_events: usize,
    // This is an exact sample assertion, not a wall-clock/real-device deadline claim.
    unexpected_audio_samples: usize,
    unexpected_midi_events: usize,
    processor_replacements: u32,
}

/// Validate a real isolated CLAP fixture using null/headless paced process calls.
/// The CLI must install startup runtime isolation before calling this function.
/// Prints one JSON object per successful case and a final result on stdout.
/// Returns failure on missing wet output, dry fallback, timestamp/order mismatch,
/// unexpected nonzero samples, unsupported identity, remote fault, or deadline.
pub fn run(format: &str, path: &Path) -> Result<()> {
    let started = Instant::now();
    let result = run_inner(format, path);
    match &result {
        Ok(()) => println!(
            "{}",
            serde_json::json!({
                "test":"isolated-clap-fixture", "outcome":"passed",
                "elapsed_ms":started.elapsed().as_millis(),
                "sample_rate":RATE as u32, "block_sizes":BLOCKS,
                "real_worker":true, "device_audio":false, "editor_tested":false,
                "fault_injection_tested":false, "process_reap_verified":false,
                "limitations":["external watchdog required", "no real-device deadline assertion", "worker cleanup has no public join/ACK API"]
            })
        ),
        Err(error) => println!(
            "{}",
            serde_json::json!({
                "test":"isolated-clap-fixture", "outcome":"failed",
                "elapsed_ms":started.elapsed().as_millis(), "error":format!("{error:#}"),
                "device_audio":false, "editor_tested":false
            })
        ),
    }
    result
}

fn run_inner(format: &str, path: &Path) -> Result<()> {
    ensure!(
        crate::runtime::get().experimental_plugin_isolation,
        "isolated bridge test requires startup runtime isolation; refusing local plugin load"
    );
    ensure!(
        format.eq_ignore_ascii_case("clap"),
        "known-DSP bridge assertions only support the explicit CLAP test fixture"
    );
    let path = std::fs::canonicalize(path).context("resolve explicit CLAP fixture path")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut host = Host::default();
    // Host::classes routes this exact path through the bounded scanner subprocess
    // under the runtime policy checked above. Do not call a format module directly.
    let classes = host.classes(Format::Clap, &path)?;
    let info = classes
        .into_iter()
        .find(|info| info.id == FIXTURE_ID)
        .context("explicit library has no org.nodeplayer.test-fixture CLAP class; no generic-plugin DSP assumptions allowed")?;
    let created = host.create(&info, RATE as f32, MAX_BLOCK)?;
    let mut harness = Harness {
        instance: created.instance,
        processor: Some(created.processor),
        inputs: [StereoBuf::new()],
        outputs: [StereoBuf::new()],
        midi_out: MidiBuf::new(),
        cursor: 0,
        block_index: 0,
        replacements: 0,
        deadline,
    };
    harness.wait_for("initial worker metadata", Duration::from_secs(30), |i| {
        i.remote_status() == Some("running")
            && [0, 1, 2, 3, 100, 101, 102, 103, 104, 105]
                .iter()
                .all(|id| i.params().iter().any(|p| p.id == *id))
    })?;
    ensure!(harness.instance.bridge_latency() == BRIDGE_LATENCY);
    ensure!(
        BRIDGE_LATENCY == 4096,
        "update this diagnostic's documented D contract before changing bridge defaults"
    );
    ensure!(
        harness.instance.midi_ports() == (true, true),
        "fixture MIDI ports not available"
    );
    for (id, value) in [(0, 0.25), (1, 0.0), (2, 1.0), (3, 0.0)] {
        harness.instance.set_param(id, value);
    }
    harness.wait_configuration(0.25, 0)?;
    harness.verify("gain-midi-latency-zero", 0.5, 0)?;

    // The fixture applies algorithm latency only after reactivation. The worker
    // must honor request_callback -> request_restart; metadata changes cause the
    // parent proxy processor replacement below. Never fake latency in the test.
    harness.instance.set_param(1, 512.0 / 65_536.0);
    harness.wait_configuration(0.25, 512)?;
    harness.verify("gain-midi-latency-512-restart", 0.5, 512)?;

    // save_state is asynchronous/cache-first. Request one checkpoint, continue
    // streaming, then inspect the returned cache (which requests another one).
    // Compare the actual fixture blob, never merely accept Some/unchanged bytes.
    let checkpoint_deadline = Instant::now() + Duration::from_secs(5);
    let mut saved = harness.instance.save_state();
    loop {
        if let Some(state) = &saved
            && fixture_state_matches(state, [0.5, 512.0, 1.0, 0.0])
        {
            break;
        }
        ensure!(
            Instant::now() < checkpoint_deadline,
            "latest fixture checkpoint did not arrive with gain=.5 and latency=512"
        );
        harness.silence(4096)?;
        saved = harness.instance.save_state();
    }
    let saved = saved.context("fixture checkpoint absent")?;
    println!(
        "{}",
        serde_json::json!({
            "test":"isolated-clap-fixture", "case":"asynchronous-state-checkpoint",
            "outcome":"passed", "state_bytes":saved.len(), "encoding_validated":true,
            "gain_native":0.5, "algorithm_latency_frames":512,
            "blob_logged":false
        })
    );

    // Prove a subsequent parameter request reached DSP, not just the optimistic
    // parent metadata cache. This also proves a later Load really restores sound.
    harness.instance.set_param(0, 0.125); // native .25
    harness.wait_configuration(0.125, 512)?;
    harness.verify("post-checkpoint-gain-change", 0.25, 512)?;
    harness.instance.load_state(&saved)?;
    harness.wait_configuration(0.25, 512)?;
    harness.verify("state-restored-gain-and-latency", 0.5, 512)?;
    Ok(())
}

fn fixture_state_matches(bytes: &[u8], expected: [f64; 4]) -> bool {
    if bytes.len() != 40 || &bytes[..8] != b"NPCLAP01" {
        return false;
    }
    expected.iter().enumerate().all(|(index, expected)| {
        let offset = 8 + index * 8;
        let actual = f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        actual == *expected
    })
}

struct Harness {
    instance: Box<dyn Instance>,
    // Option enforces dropping the old processor *before* restart allocates its replacement.
    processor: Option<Box<dyn Processor>>,
    inputs: [StereoBuf; 1],
    outputs: [StereoBuf; 1],
    midi_out: MidiBuf,
    cursor: u64,
    block_index: usize,
    replacements: u32,
    deadline: Instant,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut processor) = self.processor.take() {
            processor.deactivate();
            drop(processor); // Control thread, not a pretend real-time callback.
        }
        // Instance Drop asks its supervisor to stop. No public reap ACK exists,
        // so neither successful return nor this Drop claims process cleanup proof.
    }
}

impl Harness {
    fn housekeeping(&mut self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "isolated fixture integration exceeded 30 second deadline"
        );
        self.instance.idle();
        let status = self
            .instance
            .remote_status()
            .context("created instance is local rather than isolated")?;
        ensure!(
            !status.starts_with("faulted") && status != "stopped",
            "remote worker became {status}"
        );
        if self.instance.wants_restart() {
            if let Some(mut old) = self.processor.take() {
                old.deactivate();
                drop(old);
            }
            self.processor = Some(self.instance.restart(RATE, MAX_BLOCK)?);
            self.replacements += 1;
        }
        Ok(())
    }

    fn next_block(&mut self, remaining: u64) -> usize {
        let n = BLOCKS[self.block_index % BLOCKS.len()].min(remaining as usize);
        self.block_index += 1;
        n
    }

    fn process(&mut self, n: usize, midi: &[MidiEvent]) -> Result<()> {
        self.outputs[0].l[..n].fill(f32::NAN);
        self.outputs[0].r[..n].fill(f32::NAN);
        self.midi_out.clear();
        self.processor
            .as_mut()
            .context("remote processor missing")?
            .process_timed(
                self.cursor,
                n,
                &mut self.inputs,
                &mut self.outputs,
                midi,
                &mut self.midi_out,
            );
        self.cursor += n as u64;
        // Deliberately no wall-clock catch-up/burst after slow control operations:
        // every call gives the real worker at least its block's 48-kHz interval.
        // This is a correctness test, not a device/OS scheduling benchmark.
        thread::sleep(Duration::from_secs_f64(n as f64 / RATE));
        Ok(())
    }

    fn silent_block(&mut self, n: usize) -> Result<()> {
        self.housekeeping()?;
        self.inputs[0].clear(n);
        self.process(n, &[])
    }

    fn silence(&mut self, frames: u64) -> Result<()> {
        let end = self.cursor + frames;
        while self.cursor < end {
            let n = self.next_block(end - self.cursor);
            self.silent_block(n)?;
        }
        self.housekeeping()
    }

    fn wait_for(
        &mut self,
        label: &str,
        timeout: Duration,
        predicate: impl Fn(&dyn Instance) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.housekeeping()?;
            if predicate(self.instance.as_ref()) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "timeout waiting for {label}; remote status={:?}, total latency={}",
                self.instance.remote_status(),
                self.instance.latency()
            );
            let n = self.next_block(1024);
            self.silent_block(n)?;
        }
    }

    fn wait_configuration(&mut self, normalized_gain: f64, latency: u32) -> Result<()> {
        self.wait_for(
            "fixture parameters/active latency",
            Duration::from_secs(5),
            |instance| {
                instance.remote_status() == Some("running")
                    && (instance.param_value(0) - normalized_gain).abs() < 1e-9
                    && (instance.param_value(1) - latency as f64 / 65_536.0).abs() < 1e-9
                    && instance.param_value(2) == 1.0
                    && instance.param_value(3) == 0.0
                    && instance.latency() == BRIDGE_LATENCY + latency
            },
        )
    }

    fn verify(&mut self, case: &'static str, gain: f32, algorithm: u32) -> Result<()> {
        // Flush all audio/MIDI from startup, state quiesce and processor replacement.
        // Never send a probe while the bridge is still using its expected prefill.
        self.silence((BRIDGE_LATENCY + algorithm) as u64 + 4096)?;
        self.wait_configuration((gain / 2.0) as f64, algorithm)?;
        let replacements = self.replacements;
        let start = self.cursor;
        // Several distinct impulses straddle the worker Q=256 boundary and span
        // callback boundaries. Nonuniform amplitudes detect repeats/reordering.
        let boundary_base = (256 - start % 256) % 256;
        let offsets =
            [1024u64, 1279, 1280, 1281, 2047, 2560, 3073].map(|offset| offset + boundary_base);
        let amplitudes = [1.0f32, -0.75, 0.5, -0.25, 0.875, -0.625, 0.375];
        let probes: Vec<_> = offsets
            .into_iter()
            .zip(amplitudes)
            .map(|(offset, amplitude)| (start + offset, amplitude))
            .collect();
        let mut midi_inputs = Vec::new();
        for (index, (at, _)) in probes.iter().enumerate() {
            // Simultaneous pairs specifically test stable same-sample event order.
            midi_inputs.push((*at, [0x90, 60 + index as u8, 70 + index as u8]));
            midi_inputs.push((*at, [0x80, 60 + index as u8, index as u8]));
        }
        let expected_midi: Vec<_> = midi_inputs
            .iter()
            .map(|(at, data)| (at + BRIDGE_LATENCY as u64, *data))
            .collect();
        let mut actual_midi = Vec::new();
        let mut seen_impulses = 0;
        let tested_frames = BRIDGE_LATENCY as u64 + algorithm as u64 + 12_288;
        let end = start + tested_frames;
        while self.cursor < end {
            self.housekeeping()?;
            ensure!(
                self.replacements == replacements,
                "{case}: unexpected processor replacement during measured probe"
            );
            ensure!(
                self.instance.latency() == BRIDGE_LATENCY + algorithm,
                "{case}: latency changed during measured probe"
            );
            let n = self.next_block(end - self.cursor);
            let first = self.cursor;
            self.inputs[0].clear(n);
            for &(at, amplitude) in &probes {
                if at >= first && at < first + n as u64 {
                    let offset = (at - first) as usize;
                    self.inputs[0].l[offset] = amplitude;
                    self.inputs[0].r[offset] = -amplitude;
                }
            }
            let midi: Vec<_> = midi_inputs
                .iter()
                .filter(|(at, _)| *at >= first && *at < first + n as u64)
                .map(|(at, data)| MidiEvent {
                    time: (at - first) as u32,
                    data: *data,
                })
                .collect();
            self.process(n, &midi)?;
            for index in 0..n {
                let at = first + index as u64;
                let expected = probes
                    .iter()
                    .find(|(input_at, _)| {
                        *input_at + BRIDGE_LATENCY as u64 + algorithm as u64 == at
                    })
                    .map_or(0.0, |(_, amplitude)| amplitude * gain);
                let left = self.outputs[0].l[index];
                let right = self.outputs[0].r[index];
                if !left.is_finite()
                    || !right.is_finite()
                    || (left - expected).abs() > SAMPLE_EPSILON
                    || (right + expected).abs() > SAMPLE_EPSILON
                {
                    bail!(
                        "{case}: sample {at}: got L={left}, R={right}; expected L={expected}, R={}; delayed dry fallback is NOT a pass",
                        -expected
                    );
                }
                if expected != 0.0 {
                    seen_impulses += 1;
                }
            }
            for event in self.midi_out.events() {
                ensure!(
                    (event.time as usize) < n,
                    "{case}: MIDI offset outside callback"
                );
                actual_midi.push((first + event.time as u64, event.data));
            }
        }
        ensure!(
            seen_impulses == probes.len(),
            "{case}: measured interval missed expected impulses"
        );
        ensure!(
            actual_midi == expected_midi,
            "{case}: MIDI mismatch (timestamps/order/bytes); expected {expected_midi:?}, observed {actual_midi:?}"
        );
        let observed = Observation {
            case,
            passed: true,
            gain_native: gain,
            algorithm_latency_frames: algorithm,
            bridge_latency_frames: BRIDGE_LATENCY,
            first_sample: start,
            tested_frames,
            verified_audio_impulses: seen_impulses,
            verified_midi_events: actual_midi.len(),
            unexpected_audio_samples: 0,
            unexpected_midi_events: 0,
            processor_replacements: replacements,
        };
        println!("{}", serde_json::to_string(&observed)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_exact_fixture_blob_not_a_parent_cache_presence() {
        let expected = [0.5f64, 512.0, 1.0, 0.0];
        let mut state = b"NPCLAP01".to_vec();
        for value in expected {
            state.extend(value.to_le_bytes());
        }
        assert!(fixture_state_matches(&state, expected));
        assert!(!fixture_state_matches(&state, [0.25, 512.0, 1.0, 0.0]));
        assert!(!fixture_state_matches(&state[..39], expected));
        state[0] = 0;
        assert!(!fixture_state_matches(&state, expected));
    }
}
