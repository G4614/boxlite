//! Percentile + aggregate helpers for the bench harness.
//!
//! Self-contained: zero crate-external deps, no `criterion`, no
//! `statistical`. Bench data is small (`runs` is typically 10–100), so
//! the textbook nearest-rank percentile + a single-pass mean/stdev is
//! more than enough, and not pulling a dep keeps `boxlite-cli`'s
//! compile graph honest.

use super::report::MetricAggregate;

/// Compute the full `MetricAggregate` (min/p50/p90/p99/max/mean/stdev)
/// for a single metric over its sample list. The caller is responsible
/// for sample filtering (e.g., dropping warmup runs) — this helper
/// takes whatever it's handed and aggregates it.
///
/// Returns `None` for an empty sample set so the caller can decide
/// whether to emit a placeholder, skip the metric, or fail loudly.
/// Reporting "p99=NaN, n=0" would be worse than absence.
pub fn aggregate(name: &str, samples: &[f64]) -> Option<MetricAggregate> {
    if samples.is_empty() {
        return None;
    }

    let mut sorted: Vec<f64> = samples.iter().copied().filter(|x| !x.is_nan()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let n = sorted.len();
    let mean: f64 = sorted.iter().sum::<f64>() / n as f64;

    // Sample (Bessel-corrected) stdev. n=1 → 0.0 by definition.
    let stdev = if n > 1 {
        let var: f64 = sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        var.sqrt()
    } else {
        0.0
    };

    Some(MetricAggregate {
        name: name.to_string(),
        unit: super::report::BenchReport::unit_hint(name).to_string(),
        higher_is_better: super::report::BenchReport::higher_is_better(name),
        min: sorted[0],
        p50: nearest_rank(&sorted, 50),
        p90: nearest_rank(&sorted, 90),
        p99: nearest_rank(&sorted, 99),
        max: sorted[n - 1],
        mean,
        stdev,
        n,
    })
}

/// Nearest-rank percentile over a pre-sorted slice.
///
/// Definition (ISO 16269-4): rank = ceil(p/100 * n), 1-indexed.
/// Equivalent to Python's `numpy.percentile(... method="lower")` for
/// integer percentiles. Picked over linear interpolation because it
/// always returns a real observed sample, which matches the "tail
/// behavior" question p99 is supposed to answer ("what does my 99th
/// slowest run look like?") instead of a synthesized number.
fn nearest_rank(sorted: &[f64], percentile: u32) -> f64 {
    debug_assert!(!sorted.is_empty(), "caller must guard empty");
    debug_assert!(percentile <= 100, "percentile must be ≤ 100");
    let n = sorted.len();
    // ceil(p/100 * n), then clamp to [1, n] and convert to 0-indexed.
    let rank = ((percentile as f64 / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// p99 on a 100-sample run should land on the 99th observed value
    /// (1-indexed), per the nearest-rank definition. Production code
    /// reads aggregates back into the comparator — getting this wrong
    /// would silently inflate or deflate every regression check.
    #[test]
    fn percentile_uses_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let agg = aggregate("v_ms", &samples).expect("non-empty");
        assert_eq!(agg.p50, 50.0, "p50 of 1..=100 == 50");
        assert_eq!(agg.p90, 90.0, "p90 of 1..=100 == 90");
        assert_eq!(agg.p99, 99.0, "p99 of 1..=100 == 99");
        assert_eq!(agg.min, 1.0);
        assert_eq!(agg.max, 100.0);
        assert_eq!(agg.mean, 50.5);
        assert_eq!(agg.n, 100);
    }

    /// Single-sample sets: every percentile collapses to that sample,
    /// stdev is 0 (n-1 denominator avoids div-by-zero), unit_hint is
    /// preserved.
    #[test]
    fn aggregate_handles_single_sample() {
        let agg = aggregate("rss_bytes", &[12345.0]).expect("non-empty");
        assert_eq!(agg.min, 12345.0);
        assert_eq!(agg.p50, 12345.0);
        assert_eq!(agg.p99, 12345.0);
        assert_eq!(agg.max, 12345.0);
        assert_eq!(agg.mean, 12345.0);
        assert_eq!(agg.stdev, 0.0);
        assert_eq!(agg.unit, "bytes");
        assert_eq!(agg.n, 1);
    }

    #[test]
    fn aggregate_returns_none_on_empty() {
        assert!(aggregate("x_ms", &[]).is_none());
    }

    /// NaNs are silently dropped: they aren't a legitimate measurement
    /// from any scenario we ship (durations and resource counts are
    /// finite by construction), and propagating them would poison
    /// every downstream sort/sum.
    #[test]
    fn aggregate_drops_nan_samples() {
        let samples = [1.0, f64::NAN, 2.0, 3.0];
        let agg = aggregate("v_ms", &samples).expect("non-nan remaining");
        assert_eq!(agg.n, 3);
        assert_eq!(agg.min, 1.0);
        assert_eq!(agg.max, 3.0);
    }

    /// Stdev uses Bessel correction (`n-1` denominator) — the
    /// distinction from population stdev (`n` denominator) is what
    /// this test pins. Samples `[2, 4, 4, 4, 5, 5, 7, 9]`, mean=5:
    /// squared deviations sum to 32; sample variance = 32/7 ≈ 4.5714;
    /// sample stdev = sqrt(32/7) ≈ 2.1381. Population stdev (the
    /// thing we are NOT computing) would be sqrt(32/8) = 2.0 —
    /// asserting 2.1381 catches a silent flip back to the `n`
    /// denominator.
    #[test]
    fn stdev_uses_bessel_correction() {
        let samples = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let agg = aggregate("v_ms", &samples).expect("non-empty");
        let expected = (32.0_f64 / 7.0).sqrt();
        assert!(
            (agg.stdev - expected).abs() < 1e-9,
            "expected sample stdev={} (sqrt(32/7)), got {}; \
             a value of 2.0 here would mean we silently flipped to \
             population stdev (n denominator)",
            expected,
            agg.stdev,
        );
    }
}
