use once_cell::sync::Lazy;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

pub static JOBS_DISPATCHED: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("fc_jobs_dispatched_total", "Total jobs dispatched");
    let counter = IntCounterVec::new(opts, &["repo"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static JOBS_COMPLETED: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("fc_jobs_completed_total", "Total jobs completed successfully");
    let counter = IntCounterVec::new(opts, &["repo"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static JOBS_FAILED: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("fc_jobs_failed_total", "Total jobs failed");
    let counter = IntCounterVec::new(opts, &["repo"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static JOBS_ACTIVE: Lazy<IntGauge> = Lazy::new(|| {
    let gauge = IntGauge::new("fc_jobs_active", "Currently active jobs").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

pub static VM_BOOT_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new(
        "fc_vm_boot_duration_seconds",
        "VM boot + job execution duration in seconds",
    )
    .buckets(vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]);
    let hist = HistogramVec::new(opts, &["repo"]).unwrap();
    REGISTRY.register(Box::new(hist.clone())).unwrap();
    hist
});

pub static GITHUB_API_CALLS: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("fc_github_api_calls_total", "Total GitHub API calls");
    let counter = IntCounterVec::new(opts, &["endpoint"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static GITHUB_RATE_LIMIT_REMAINING: Lazy<IntGauge> = Lazy::new(|| {
    let gauge = IntGauge::new(
        "fc_github_rate_limit_remaining",
        "GitHub API rate limit remaining",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

pub static POOL_SLOTS_AVAILABLE: Lazy<IntGauge> = Lazy::new(|| {
    let gauge = IntGauge::new("fc_pool_slots_available", "Available VM slots").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

pub static POLL_CYCLES: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("fc_poll_cycles_total", "Total poll cycles");
    let counter = IntCounterVec::new(opts, &["status"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static UPTIME_SECONDS: Lazy<GaugeVec> = Lazy::new(|| {
    let opts = Opts::new("fc_uptime_seconds", "Process uptime in seconds");
    let gauge = GaugeVec::new(opts, &["version"]).unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Render all registered metrics as Prometheus text format.
pub fn gather() -> String {
    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
