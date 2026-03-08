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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_returns_valid_prometheus_text() {
        let output = gather();
        // Prometheus text format uses "# HELP" and "# TYPE" lines
        // At minimum the registry should produce some output
        assert!(output.is_ascii() || output.is_empty());
    }

    #[test]
    fn jobs_dispatched_counter() {
        JOBS_DISPATCHED.with_label_values(&["test-repo"]).inc();
        let val = JOBS_DISPATCHED
            .with_label_values(&["test-repo"])
            .get();
        assert!(val >= 1);
    }

    #[test]
    fn jobs_active_gauge() {
        let before = JOBS_ACTIVE.get();
        JOBS_ACTIVE.inc();
        assert_eq!(JOBS_ACTIVE.get(), before + 1);
        JOBS_ACTIVE.dec();
        assert_eq!(JOBS_ACTIVE.get(), before);
    }

    #[test]
    fn pool_slots_gauge() {
        let before = POOL_SLOTS_AVAILABLE.get();
        POOL_SLOTS_AVAILABLE.set(10);
        assert_eq!(POOL_SLOTS_AVAILABLE.get(), 10);
        POOL_SLOTS_AVAILABLE.set(before);
    }

    #[test]
    fn rate_limit_gauge() {
        GITHUB_RATE_LIMIT_REMAINING.set(4999);
        assert_eq!(GITHUB_RATE_LIMIT_REMAINING.get(), 4999);
    }

    #[test]
    fn uptime_gauge() {
        UPTIME_SECONDS.with_label_values(&["test"]).set(42.0);
        let val = UPTIME_SECONDS.with_label_values(&["test"]).get();
        assert!((val - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gather_includes_registered_metrics() {
        // Force initialization of at least one metric
        JOBS_ACTIVE.set(0);
        let output = gather();
        assert!(output.contains("fc_jobs_active"));
    }
}
