pub mod ble;
pub mod gpio;
pub mod interval;

use anyhow::Result;
use ble::BleClient;
use gpio::{level_to_fact, BeamDebouncer, EdgeDecision, GpioReader, BEAM_ACTIVE_LOW, REFRACTORY};
use interval::IntervalControl;
use tracing::{info, warn};

pub async fn run_loop(
    mut client: Box<dyn BleClient>,
    mut reader: Box<dyn GpioReader>,
    interval: Box<dyn IntervalControl>,
) -> Result<()> {
    client.connect().await?;
    info!("Connected successfully!");

    if let Err(e) = interval.enforce().await {
        warn!("interval enforcement failed: {e:#}");
    }
    if let Err(e) = interval.start_guard() {
        warn!("interval guard unavailable: {e:#}");
    }

    let mut fact = level_to_fact(reader.read_level()?, BEAM_ACTIVE_LOW);
    client.write_state(fact).await?;

    let mut debouncer = BeamDebouncer::new(REFRACTORY);

    loop {
        let now = tokio::time::Instant::now();
        let edge = match debouncer.remaining(now) {
            Some(window) => match tokio::time::timeout(window, reader.wait_for_edge()).await {
                Ok(edge) => Some(edge),
                Err(_elapsed) => None,
            },
            None => Some(reader.wait_for_edge().await),
        };

        match edge {
            Some(Ok(edge)) => {
                let now = tokio::time::Instant::now();
                if debouncer.on_edge(now) == EdgeDecision::Accept {
                    info!(
                        kernel_ts_ns = edge.timestamp_ns,
                        level = ?edge.level,
                        "accepted beam edge"
                    );
                    let next = level_to_fact(edge.level, BEAM_ACTIVE_LOW);
                    if next != fact {
                        fact = next;
                        client.write_state(fact).await?;
                    }
                }
            }
            Some(Err(e)) => return Err(e),
            None => {
                let now = tokio::time::Instant::now();
                if debouncer.window_expired(now) {
                    let sampled = level_to_fact(reader.read_level()?, BEAM_ACTIVE_LOW);
                    if sampled != fact {
                        fact = sampled;
                        client.write_state(fact).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::MockBleClient;
    use crate::gpio::{GpioEdge, Level, MockGpioReader};
    use crate::interval::MockIntervalControl;
    use async_trait::async_trait;
    use mockall::predicate::eq;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn edge(level: Level) -> GpioEdge {
        GpioEdge {
            level,
            timestamp_ns: 42,
        }
    }

    fn passing_interval() -> MockIntervalControl {
        let mut interval = MockIntervalControl::new();
        interval.expect_enforce().times(1).returning(|| Ok(()));
        interval.expect_start_guard().times(1).returning(|| Ok(()));
        interval
    }

    #[tokio::test]
    async fn startup_samples_and_writes_before_any_edge() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(
            Box::new(client),
            Box::new(gpio),
            Box::new(passing_interval()),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn accepted_edge_writes_once_when_level_differs() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));

        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        client
            .expect_write_state()
            .with(eq(false))
            .times(1)
            .returning(|_| Ok(()));

        let res = run_loop(
            Box::new(client),
            Box::new(gpio),
            Box::new(passing_interval()),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test(start_paused = true)]
    async fn suppressed_burst_writes_once() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));

        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        client
            .expect_write_state()
            .with(eq(false))
            .times(1)
            .returning(|_| Ok(()));

        let res = run_loop(
            Box::new(client),
            Box::new(gpio),
            Box::new(passing_interval()),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    /// `MockGpioReader` cannot make `wait_for_edge` pend: mockall's
    /// `#[async_trait]` expectation returns the output value directly, so the
    /// generated future is always immediately ready. Exercising the loop's
    /// `None` (timeout) branch therefore needs a reader whose second
    /// `wait_for_edge` stays pending past the 20 ms window.
    struct FakePersistentReader {
        read_level_calls: Arc<AtomicUsize>,
        wait_for_edge_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl GpioReader for FakePersistentReader {
        async fn wait_for_edge(&mut self) -> Result<GpioEdge> {
            match self.wait_for_edge_calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(edge(Level::Low)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(edge(Level::High))
                }
                _ => Err(anyhow::anyhow!("stop")),
            }
        }

        fn read_level(&mut self) -> Result<Level> {
            let call = self.read_level_calls.fetch_add(1, Ordering::SeqCst);
            Ok(if call == 0 { Level::Low } else { Level::High })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refractory_timeout_resamples_and_adopts_changed_level() {
        let mut client = MockBleClient::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        client
            .expect_write_state()
            .with(eq(false))
            .times(1)
            .returning(|_| Ok(()));

        let read_level_calls = Arc::new(AtomicUsize::new(0));
        let wait_for_edge_calls = Arc::new(AtomicUsize::new(0));
        let gpio = FakePersistentReader {
            read_level_calls: Arc::clone(&read_level_calls),
            wait_for_edge_calls: Arc::clone(&wait_for_edge_calls),
        };

        let res = run_loop(
            Box::new(client),
            Box::new(gpio),
            Box::new(passing_interval()),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
        assert_eq!(read_level_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn startup_connect_enforce_guard_before_first_write() {
        let mut sequence = mockall::Sequence::new();
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();
        let mut interval = MockIntervalControl::new();

        client
            .expect_connect()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| Ok(()));
        interval
            .expect_enforce()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| Ok(()));
        interval
            .expect_start_guard()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(Box::new(client), Box::new(gpio), Box::new(interval)).await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn enforce_error_does_not_abort_run_loop() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();
        let mut interval = MockIntervalControl::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        interval.expect_enforce().times(1).returning(|| {
            Err(anyhow::anyhow!(
                "LE Connection Update rejected, status 0x3a"
            ))
        });
        interval.expect_start_guard().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(Box::new(client), Box::new(gpio), Box::new(interval)).await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn start_guard_error_does_not_abort_run_loop() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();
        let mut interval = MockIntervalControl::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        interval.expect_enforce().times(1).returning(|| Ok(()));
        interval
            .expect_start_guard()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("bind HCI monitor socket: EINVAL")));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(Box::new(client), Box::new(gpio), Box::new(interval)).await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }
}
