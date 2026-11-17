//! Opt-in measurement only; no phase events or clocks in ordinary builds.

pub(crate) struct Phase {
    #[cfg(feature = "benchmark-profiling")]
    name: &'static str,
    #[cfg(feature = "benchmark-profiling")]
    started: std::time::Instant,
}

impl Phase {
    #[inline]
    pub(crate) fn start(_name: &'static str) -> Self {
        #[cfg(feature = "benchmark-profiling")]
        {
            let started = std::time::Instant::now();
            emit(_name, "begin", None);
            Self {
                name: _name,
                started,
            }
        }
        #[cfg(not(feature = "benchmark-profiling"))]
        Self {}
    }
}

#[cfg(feature = "benchmark-profiling")]
fn emit(name: &str, event: &str, elapsed_ms: Option<f64>) {
    use std::io::Write;
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        serde_json::json!({
            "benchmark_phase": name,
            "event": event,
            "since_first_phase_ms": start.elapsed().as_secs_f64() * 1000.0,
            "elapsed_ms": elapsed_ms,
        })
    );
}

impl Drop for Phase {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "benchmark-profiling")]
        emit(
            self.name,
            "end",
            Some(self.started.elapsed().as_secs_f64() * 1000.0),
        );
    }
}

/// Accumulate small operations without per-operation logging or millisecond
/// truncation. Ordinary builds execute only the supplied operation.
pub(crate) struct Accumulator {
    #[cfg(feature = "benchmark-profiling")]
    name: &'static str,
    #[cfg(feature = "benchmark-profiling")]
    elapsed: std::time::Duration,
    #[cfg(feature = "benchmark-profiling")]
    count: u64,
}

impl Accumulator {
    #[inline]
    pub(crate) fn new(_name: &'static str) -> Self {
        Self {
            #[cfg(feature = "benchmark-profiling")]
            name: _name,
            #[cfg(feature = "benchmark-profiling")]
            elapsed: std::time::Duration::ZERO,
            #[cfg(feature = "benchmark-profiling")]
            count: 0,
        }
    }

    #[inline]
    pub(crate) fn measure<T>(&mut self, operation: impl FnOnce() -> T) -> T {
        #[cfg(feature = "benchmark-profiling")]
        let started = std::time::Instant::now();
        let result = operation();
        #[cfg(feature = "benchmark-profiling")]
        {
            self.elapsed += started.elapsed();
            self.count += 1;
        }
        result
    }
}

impl Drop for Accumulator {
    fn drop(&mut self) {
        #[cfg(feature = "benchmark-profiling")]
        {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "{}",
                serde_json::json!({
                    "benchmark_total": self.name,
                    "count": self.count,
                    "elapsed_ms": self.elapsed.as_secs_f64() * 1000.0,
                })
            );
        }
    }
}
