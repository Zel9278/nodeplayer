//! Bounded RT instrumentation: relaxed atomics only, no clocks, allocation, locks or I/O.
//! The caller supplies timings measured by its own monotonic clock. Percentiles are
//! upper bounds from logarithmic buckets, not fabricated exact sample quantiles.
use crate::runtime::MetricsMode;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
const BUCKETS: usize = 64;
struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    max: AtomicU64,
}
impl Histogram {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            max: AtomicU64::new(0),
        }
    }
    fn observe(&self, value: u64) {
        let index = if value == 0 {
            0
        } else {
            (64 - value.leading_zeros() as usize).min(BUCKETS - 1)
        };
        self.buckets[index].fetch_add(1, Relaxed);
        self.max.fetch_max(value, Relaxed);
    }
    fn snapshot(&self) -> HistogramSnapshot {
        let buckets: Vec<_> = self.buckets.iter().map(|b| b.load(Relaxed)).collect();
        let count = buckets.iter().copied().fold(0u64, u64::saturating_add);
        let percentile = |percent: u64| -> Option<u64> {
            if count == 0 {
                return None;
            }
            let target = count.saturating_mul(percent).saturating_add(99) / 100;
            let mut cumulative = 0u64;
            for (i, n) in buckets.iter().enumerate() {
                cumulative = cumulative.saturating_add(*n);
                if cumulative >= target {
                    return Some(if i == 63 {
                        u64::MAX
                    } else {
                        (1u64 << i).saturating_sub(1)
                    });
                }
            }
            None
        };
        HistogramSnapshot {
            count,
            p50_upper: percentile(50),
            p95_upper: percentile(95),
            p99_upper: percentile(99),
            max: (count > 0).then(|| self.max.load(Relaxed)),
            buckets,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub p50_upper: Option<u64>,
    pub p95_upper: Option<u64>,
    pub p99_upper: Option<u64>,
    pub max: Option<u64>,
    pub buckets: Vec<u64>,
}
pub struct Metrics {
    mode: MetricsMode,
    callback_ns: Histogram,
    dsp_ns: Histogram,
    period_ns: Histogram,
    callback_frames: Histogram,
    deadline_misses: AtomicU64,
    telemetry_overflows: AtomicU64,
}
impl Metrics {
    pub fn new(mode: MetricsMode) -> Self {
        Self {
            mode,
            callback_ns: Histogram::new(),
            dsp_ns: Histogram::new(),
            period_ns: Histogram::new(),
            callback_frames: Histogram::new(),
            deadline_misses: AtomicU64::new(0),
            telemetry_overflows: AtomicU64::new(0),
        }
    }
    pub fn enabled(&self) -> bool {
        self.mode != MetricsMode::Off
    }
    pub fn record_callback(
        &self,
        frames: u64,
        elapsed_ns: u64,
        period_ns: Option<u64>,
        budget_ns: u64,
    ) {
        if !self.enabled() {
            return;
        }
        self.callback_frames.observe(frames);
        self.callback_ns.observe(elapsed_ns);
        if let Some(period) = period_ns {
            self.period_ns.observe(period);
        }
        if elapsed_ns > budget_ns {
            self.deadline_misses.fetch_add(1, Relaxed);
        }
    }
    pub fn record_dsp(&self, elapsed_ns: u64) {
        if self.enabled() {
            self.dsp_ns.observe(elapsed_ns);
        }
    }
    pub fn telemetry_overflow(&self) {
        if self.enabled() {
            self.telemetry_overflows.fetch_add(1, Relaxed);
        }
    }
    /// Non-RT only. Concurrent snapshots are approximate, not a consistent transaction.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            mode: self.mode,
            quantiles: "log2-bucket-upper-bounds",
            callback_ns: self.callback_ns.snapshot(),
            dsp_ns: self.dsp_ns.snapshot(),
            period_ns: self.period_ns.snapshot(),
            callback_frames: self.callback_frames.snapshot(),
            deadline_misses: self.deadline_misses.load(Relaxed),
            // Remote counters are per-instance and emitted in engine-progress/worker-status.
            // No aggregate collector is connected yet; zero here would claim false coverage.
            bridge_underruns: None,
            overflows: None,
            control_timeouts: None,
            telemetry_overflows: self.telemetry_overflows.load(Relaxed),
            device_xruns: None,
            cpu_percent: None,
            rss_bytes: None,
            ui_response_ns: None,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub mode: MetricsMode,
    pub quantiles: &'static str,
    pub callback_ns: HistogramSnapshot,
    pub dsp_ns: HistogramSnapshot,
    pub period_ns: HistogramSnapshot,
    pub callback_frames: HistogramSnapshot,
    pub deadline_misses: u64,
    pub bridge_underruns: Option<u64>,
    pub overflows: Option<u64>,
    pub control_timeouts: Option<u64>,
    pub telemetry_overflows: u64,
    /// None means unavailable, never inferred from callback deadlines.
    pub device_xruns: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub ui_response_ns: Option<u64>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_histogram_and_distinct_deadlines() {
        let m = Metrics::new(MetricsMode::Minimal);
        m.record_callback(512, 150, Some(1000), 100);
        let s = m.snapshot();
        assert_eq!(s.deadline_misses, 1);
        assert_eq!(s.device_xruns, None);
        assert_eq!(s.callback_ns.p99_upper, Some(255));
        assert_eq!(s.callback_ns.max, Some(150));
        assert_eq!(s.bridge_underruns, None);
    }
    #[test]
    fn off_collects_nothing() {
        let m = Metrics::new(MetricsMode::Off);
        m.record_dsp(5);
        assert_eq!(m.snapshot().dsp_ns.count, 0);
        assert_eq!(m.snapshot().overflows, None);
    }
}
