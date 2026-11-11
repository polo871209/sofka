//! When to poll `metrics.k8s.io` next.
//!
//! metrics-server publishes a new sample once per `--metric-resolution`
//! (15 s upstream, 30 s on GKE), so polls between two samples return the same
//! data. The poller learns the period from the server-side `timestamp` of the
//! newest sample and the phase from the local time at which it first saw that
//! sample. It polls again `LEAD` before the next sample is due, then once more
//! `RETRY` later if the sample was not there yet.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The poll interval before the period is known, and after repeated misses.
pub(super) const FLOOR: Duration = Duration::from_secs(5);
const LEAD: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(2);
const MAX: Duration = Duration::from_secs(60);
const PERIODS: usize = 4;

#[derive(Default)]
pub(super) struct MetricsCadence {
    newest: Option<i128>,
    /// Local time the current `newest` sample was first seen. `None` until a
    /// change is seen, because the first sample's age is unknown.
    changed_at: Option<Instant>,
    periods: VecDeque<i128>,
    misses: u32,
}

impl MetricsCadence {
    /// Record the result of a poll that finished at `now` and return the
    /// delay until the next poll. `newest` is the newest sample timestamp in
    /// nanoseconds, or `None` when the poll failed or returned no samples.
    pub(super) fn next_delay(&mut self, newest: Option<i128>, now: Instant) -> Duration {
        let Some(newest) = newest else {
            return FLOOR;
        };
        match self.newest {
            Some(previous) if previous == newest => {}
            previous => {
                if let Some(previous) = previous {
                    // Several metrics-server replicas can answer out of order.
                    if newest > previous {
                        if self.periods.len() == PERIODS {
                            self.periods.pop_front();
                        }
                        self.periods.push_back(newest - previous);
                    }
                    self.changed_at = Some(now);
                }
                self.newest = Some(newest);
                self.misses = 0;
            }
        }
        // The smallest gap is the period. A larger one means a missed sample.
        let (Some(changed_at), Some(&period)) = (self.changed_at, self.periods.iter().min()) else {
            return FLOOR;
        };
        let period = Duration::from_nanos(u64::try_from(period).unwrap_or(u64::MAX));
        let due = changed_at + period.saturating_sub(LEAD).clamp(FLOOR, MAX);
        if now < due {
            return due - now;
        }
        self.misses += 1;
        if self.misses == 1 { RETRY } else { FLOOR }
    }
}

/// The newest `timestamp` among metrics items, in nanoseconds.
pub(super) fn newest_sample<'a>(
    items: impl IntoIterator<Item = &'a kube::core::DynamicObject>,
) -> Option<i128> {
    items
        .into_iter()
        .filter_map(|item| item.data.get("timestamp")?.as_str())
        .filter_map(|ts| ts.parse::<k8s_openapi::jiff::Timestamp>().ok())
        .map(|ts| ts.as_nanosecond())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i128 = 1_000_000_000;

    /// Simulate a server that publishes every `period` seconds, starting at
    /// `offset`, and return the poll times and each sample's delivery lag.
    fn simulate(period: f64, offset: f64, seconds: f64) -> (Vec<f64>, Vec<f64>) {
        let start = Instant::now();
        let mut cadence = MetricsCadence::default();
        let mut t = 0.0;
        let mut polls = Vec::new();
        let mut lags = Vec::new();
        let mut last_seen = None;
        while t < seconds {
            polls.push(t);
            let published = ((t - offset) / period).floor();
            let newest = (t >= offset).then_some(published as i128);
            if newest.is_some() && newest != last_seen {
                if last_seen.is_some() {
                    lags.push(t - (offset + published * period));
                }
                last_seen = newest;
            }
            let at = start + Duration::from_secs_f64(t);
            let ts = newest.map(|n| (offset + n as f64 * period) as i128 * S);
            t += cadence.next_delay(ts, at).as_secs_f64();
        }
        (polls, lags)
    }

    #[test]
    fn locks_to_a_thirty_second_period() {
        let (polls, lags) = simulate(30.0, 3.7, 600.0);
        // The old poller ran every 5 s: 120 polls in 600 s.
        assert!(polls.len() <= 45, "{} polls", polls.len());
        let steady = &lags[2..];
        assert!(steady.iter().all(|l| *l <= 2.0 + 1e-9), "{lags:?}");
    }

    #[test]
    fn fifteen_second_period_needs_fewer_polls_and_no_more_lag() {
        let (polls, lags) = simulate(15.0, 1.3, 600.0);
        assert!(polls.len() <= 85, "{} polls", polls.len());
        assert!(lags[2..].iter().all(|l| *l <= 2.0 + 1e-9), "{lags:?}");
    }

    #[test]
    fn fast_or_stuck_servers_poll_at_the_floor() {
        let (polls, _) = simulate(1.0, 0.0, 60.0);
        assert_eq!(polls.len(), 12);
        let mut cadence = MetricsCadence::default();
        let now = Instant::now();
        for i in 0..10 {
            let delay = cadence.next_delay(Some(7 * S), now + FLOOR * i);
            assert_eq!(delay, FLOOR);
        }
    }

    #[test]
    fn failed_polls_and_reordered_replicas_do_not_shorten_the_interval() {
        let mut cadence = MetricsCadence::default();
        let now = Instant::now();
        assert_eq!(cadence.next_delay(None, now), FLOOR);
        cadence.next_delay(Some(100 * S), now);
        cadence.next_delay(Some(130 * S), now + Duration::from_secs(5));
        // An older sample from another replica is not a new period.
        let delay = cadence.next_delay(Some(110 * S), now + Duration::from_secs(10));
        assert_eq!(cadence.periods, [30 * S]);
        assert!(delay >= FLOOR, "{delay:?}");
    }

    #[test]
    fn newest_sample_reads_the_latest_timestamp() {
        let item = |ts: &str| -> kube::core::DynamicObject {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "metrics.k8s.io/v1beta1", "kind": "PodMetrics",
                "metadata": {"name": "p"}, "timestamp": ts, "window": "30s"
            }))
            .unwrap()
        };
        let items = [
            item("2026-10-01T00:34:30Z"),
            item("2026-10-01T00:34:59Z"),
            item("bad"),
        ];
        let newest = newest_sample(&items).unwrap();
        let expected: k8s_openapi::jiff::Timestamp = "2026-10-01T00:34:59Z".parse().unwrap();
        assert_eq!(newest, expected.as_nanosecond());
        assert_eq!(newest_sample(&[]), None);
    }
}
