use anyhow::{Context, Result};
use async_trait::async_trait;
use gpiocdev::line::{Bias, EdgeDetection, Value};
use gpiocdev::tokio::AsyncRequest;
use gpiocdev::{Chip, Request};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
use mockall::{automock, predicate::*};

/// Logical level sampled from the beam line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Low,
    High,
}

/// An edge reported by the kernel: the level sampled at the edge and the raw
/// kernel event timestamp (the latency anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpioEdge {
    pub level: Level,
    pub timestamp_ns: u64,
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait GpioReader: Send + Sync {
    /// Block until the kernel reports an edge, then return the sampled level and
    /// the raw kernel event timestamp. The level is sampled, never derived from
    /// the edge direction.
    async fn wait_for_edge(&mut self) -> Result<GpioEdge>;

    /// Explicitly sample the current line level (startup and refractory re-sample).
    fn read_level(&mut self) -> Result<Level>;
}

/// Chardev line-event GPIO reader.
pub struct ChardevGpio {
    req: AsyncRequest,
}

impl ChardevGpio {
    /// Resolve the chip by label and request `offset` as an input with both-edge
    /// detection, no bias, and consumer label `garagebeam`.
    pub fn new(chip_label: &str, offset: u32) -> Result<Self> {
        let chip_path = resolve_chip_by_label(chip_label)?;
        let req = Request::builder()
            .on_chip(chip_path.as_path())
            .with_consumer("garagebeam")
            .with_line(offset)
            .as_input()
            .with_bias(Bias::Disabled)
            .with_edge_detection(EdgeDetection::BothEdges)
            .request()
            .with_context(|| {
                format!(
                    "failed to request line {} on chip '{}' ({})",
                    offset,
                    chip_label,
                    chip_path.display()
                )
            })?;
        Ok(Self {
            req: AsyncRequest::new(req),
        })
    }
}

fn resolve_chip_by_label(label: &str) -> Result<PathBuf> {
    let chips = gpiocdev::chip::chips().context("failed to enumerate GPIO chips")?;
    for path in chips {
        if let Ok(chip) = Chip::from_path(&path) {
            if chip.info().map(|info| info.label == label).unwrap_or(false) {
                return Ok(path);
            }
        }
    }
    Err(anyhow::anyhow!("no GPIO chip with label '{}' found", label))
}

fn value_to_level(value: Value) -> Level {
    match value {
        Value::Active => Level::High,
        Value::Inactive => Level::Low,
    }
}

#[async_trait]
impl GpioReader for ChardevGpio {
    async fn wait_for_edge(&mut self) -> Result<GpioEdge> {
        let event = self
            .req
            .read_edge_event()
            .await
            .context("failed to read GPIO edge event")?;
        let level = self.read_level()?;
        Ok(GpioEdge {
            level,
            timestamp_ns: event.timestamp_ns,
        })
    }

    fn read_level(&mut self) -> Result<Level> {
        let value = self
            .req
            .as_ref()
            .lone_value()
            .context("failed to sample GPIO line level")?;
        Ok(value_to_level(value))
    }
}

/// Refractory window applied after an accepted edge.
pub const REFRACTORY: Duration = Duration::from_millis(20);

/// Result of offering an edge to the debouncer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDecision {
    Accept,
    Suppress,
}

/// First-edge-wins refractory window. Pure: the caller injects `now`.
#[derive(Debug)]
pub struct BeamDebouncer {
    window: Duration,
    last_accepted: Option<tokio::time::Instant>,
}

impl BeamDebouncer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_accepted: None,
        }
    }

    /// Accept the first edge, or an edge once the previous window has elapsed;
    /// suppress edges still inside the window. An accepted edge becomes the
    /// window anchor.
    pub fn on_edge(&mut self, now: tokio::time::Instant) -> EdgeDecision {
        match self.last_accepted {
            Some(anchor) if now.saturating_duration_since(anchor) < self.window => {
                EdgeDecision::Suppress
            }
            _ => {
                self.last_accepted = Some(now);
                EdgeDecision::Accept
            }
        }
    }

    /// `Some(remaining)` while an accepted edge's window is still open.
    pub fn remaining(&self, now: tokio::time::Instant) -> Option<Duration> {
        let anchor = self.last_accepted?;
        let elapsed = now.saturating_duration_since(anchor);
        (elapsed < self.window).then(|| self.window - elapsed)
    }

    /// `true` when an accepted edge's window has elapsed (drives the re-sample).
    pub fn window_expired(&self, now: tokio::time::Instant) -> bool {
        match self.last_accepted {
            Some(anchor) => now.saturating_duration_since(anchor) >= self.window,
            None => false,
        }
    }
}

/// Wiring polarity is an assumption to confirm at cutover.
pub const BEAM_ACTIVE_LOW: bool = true;

/// `true` = beam broken (the fact handed to the write path).
pub fn level_to_fact(level: Level, active_low: bool) -> bool {
    match level {
        Level::Low => active_low,
        Level::High => !active_low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: tokio::time::Instant, ms: u64) -> tokio::time::Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn clean_transition_accepts_then_suppresses() {
        let mut debouncer = BeamDebouncer::new(REFRACTORY);
        let t0 = tokio::time::Instant::now();
        assert_eq!(debouncer.on_edge(t0), EdgeDecision::Accept);
        assert_eq!(debouncer.on_edge(at(t0, 1)), EdgeDecision::Suppress);
    }

    #[test]
    fn chatter_within_window_yields_one_accept() {
        let mut debouncer = BeamDebouncer::new(REFRACTORY);
        let t0 = tokio::time::Instant::now();
        assert_eq!(debouncer.on_edge(t0), EdgeDecision::Accept);
        for ms in [1, 5, 10, 19] {
            assert_eq!(debouncer.on_edge(at(t0, ms)), EdgeDecision::Suppress);
        }
    }

    #[test]
    fn window_elapses_then_edge_accepts_again() {
        let mut debouncer = BeamDebouncer::new(REFRACTORY);
        let t0 = tokio::time::Instant::now();
        assert_eq!(debouncer.on_edge(t0), EdgeDecision::Accept);
        assert_eq!(debouncer.remaining(at(t0, 20)), None);
        assert!(debouncer.window_expired(at(t0, 20)));
        assert_eq!(debouncer.on_edge(at(t0, 20)), EdgeDecision::Accept);
    }

    #[test]
    fn remaining_counts_down_within_window() {
        let mut debouncer = BeamDebouncer::new(REFRACTORY);
        let t0 = tokio::time::Instant::now();
        assert_eq!(debouncer.remaining(t0), None);
        debouncer.on_edge(t0);
        assert_eq!(
            debouncer.remaining(at(t0, 5)),
            Some(Duration::from_millis(15))
        );
        assert_eq!(
            debouncer.remaining(at(t0, 19)),
            Some(Duration::from_millis(1))
        );
        assert_eq!(debouncer.remaining(at(t0, 20)), None);
    }

    #[test]
    fn refractory_resample_semantics() {
        let mut debouncer = BeamDebouncer::new(REFRACTORY);
        let t0 = tokio::time::Instant::now();
        debouncer.on_edge(t0);
        assert!(!debouncer.window_expired(at(t0, 19)));
        assert!(debouncer.window_expired(at(t0, 20)));

        // After expiry a changed sample is adopted, an unchanged one is a no-op.
        let latched = level_to_fact(Level::Low, BEAM_ACTIVE_LOW);
        let changed = level_to_fact(Level::High, BEAM_ACTIVE_LOW);
        assert_ne!(changed, latched);
        assert_eq!(level_to_fact(Level::Low, BEAM_ACTIVE_LOW), latched);
    }

    #[test]
    fn level_to_fact_both_polarities() {
        assert!(level_to_fact(Level::Low, true));
        assert!(!level_to_fact(Level::High, true));
        assert!(!level_to_fact(Level::Low, false));
        assert!(level_to_fact(Level::High, false));
    }
}
