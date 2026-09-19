pub mod ble;
pub mod gpio;
pub mod instrumentation;
pub mod interval;

use anyhow::Result;
use ble::BleClient;
use gpio::{level_to_fact, BeamDebouncer, EdgeDecision, GpioReader, BEAM_ACTIVE_LOW, REFRACTORY};
use interval::IntervalControl;
use std::time::Duration;
use tracing::{info, warn};

pub const KEEPALIVE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDecision {
    Idle,
    Write(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected,
    Steady,
}

/// Pure write schedule. `ConnectionEvent::Connected` always writes the current
/// fact (the resync after every (re)connect); `Steady` writes on a fact change
/// or when the silent keepalive is due since `last_write`.
pub fn schedule_write(
    event: ConnectionEvent,
    last_fact: Option<bool>,
    last_write: Option<tokio::time::Instant>,
    fact: bool,
    now: tokio::time::Instant,
) -> WriteDecision {
    match event {
        ConnectionEvent::Connected => WriteDecision::Write(fact),
        ConnectionEvent::Steady => {
            let changed = last_fact != Some(fact);
            let keepalive_due =
                last_write.is_none_or(|t| now.saturating_duration_since(t) >= KEEPALIVE);
            if changed || keepalive_due {
                WriteDecision::Write(fact)
            } else {
                WriteDecision::Idle
            }
        }
    }
}

pub async fn run_loop(
    mut client: Box<dyn BleClient>,
    mut reader: Box<dyn GpioReader>,
    interval: Box<dyn IntervalControl>,
    instrumentation: crate::instrumentation::Instrumentation,
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
    let mut last_fact: Option<bool> = None;
    let mut last_write: Option<tokio::time::Instant> = None;
    let now = tokio::time::Instant::now();
    if let WriteDecision::Write(value) =
        schedule_write(ConnectionEvent::Connected, None, last_write, fact, now)
    {
        client.write_state(value).await?;
        last_fact = Some(value);
        last_write = Some(now);
    }

    let mut debouncer = BeamDebouncer::new(REFRACTORY);

    loop {
        let now = tokio::time::Instant::now();
        let deadline = last_write.map_or_else(tokio::time::Instant::now, |t| t + KEEPALIVE);
        let edge = {
            let edge_wait = async {
                match debouncer.remaining(now) {
                    Some(window) => tokio::time::timeout(window, reader.wait_for_edge())
                        .await
                        .ok(),
                    None => Some(reader.wait_for_edge().await),
                }
            };
            tokio::pin!(edge_wait);
            // Unbiased: a continuously-ready edge must not starve the keepalive.
            tokio::select! {
                edge = &mut edge_wait => edge,
                result = client.wait_for_reconnect() => {
                    result?;
                    let now = tokio::time::Instant::now();
                    if let WriteDecision::Write(value) = schedule_write(
                        ConnectionEvent::Connected,
                        None,
                        last_write,
                        fact,
                        now,
                    ) {
                        client.write_state(value).await?;
                        last_fact = Some(value);
                        last_write = Some(now);
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = tokio::time::Instant::now();
                    if let WriteDecision::Write(value) = schedule_write(
                        ConnectionEvent::Steady,
                        last_fact,
                        last_write,
                        fact,
                        now,
                    ) {
                        client.write_state(value).await?;
                        last_fact = Some(value);
                        last_write = Some(now);
                    }
                    continue;
                }
            }
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
                    fact = level_to_fact(edge.level, BEAM_ACTIVE_LOW);
                    if let WriteDecision::Write(value) =
                        schedule_write(ConnectionEvent::Steady, last_fact, last_write, fact, now)
                    {
                        if instrumentation.enabled() {
                            let t_write = crate::instrumentation::monotonic_ns();
                            client.write_state(value).await?;
                            if t_write < edge.timestamp_ns {
                                tracing::warn!(
                                    target: "garage_beam::latency",
                                    edge_ns = edge.timestamp_ns,
                                    write_ns = t_write,
                                    "latency_clock_skew"
                                );
                            }
                            let edge_to_write_ns = t_write.saturating_sub(edge.timestamp_ns);
                            let t0 = crate::instrumentation::monotonic_ns();
                            let (write_to_read_ns, read_back) = match client.read_state().await {
                                Ok(byte) => (
                                    Some(crate::instrumentation::monotonic_ns().saturating_sub(t0)),
                                    Some(byte),
                                ),
                                Err(e) => {
                                    tracing::warn!(
                                        target: "garage_beam::latency",
                                        error = %e,
                                        "latency_read_failed"
                                    );
                                    (None, None)
                                }
                            };
                            match write_to_read_ns {
                                Some(b) => {
                                    tracing::info!(
                                        target: "garage_beam::latency",
                                        edge_to_write_ns,
                                        write_to_read_ns = b,
                                        read_back = read_back,
                                        "latency_sample"
                                    );
                                }
                                None => {
                                    tracing::info!(
                                        target: "garage_beam::latency",
                                        edge_to_write_ns,
                                        read_back = read_back,
                                        "latency_sample"
                                    );
                                }
                            }
                        } else {
                            client.write_state(value).await?;
                        }
                        last_fact = Some(value);
                        last_write = Some(now);
                    }
                }
            }
            Some(Err(e)) => return Err(e),
            None => {
                let now = tokio::time::Instant::now();
                if debouncer.window_expired(now) {
                    fact = level_to_fact(reader.read_level()?, BEAM_ACTIVE_LOW);
                    if let WriteDecision::Write(value) =
                        schedule_write(ConnectionEvent::Steady, last_fact, last_write, fact, now)
                    {
                        client.write_state(value).await?;
                        last_fact = Some(value);
                        last_write = Some(now);
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
    use std::sync::{Arc, Mutex};
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

    /// `MockBleClient` cannot model a pending `wait_for_reconnect`: mockall's
    /// async `returning(...)` yields an already-ready future. Existing
    /// expectation-based tests therefore run through this thin adapter, which
    /// delegates every call and overrides only `wait_for_reconnect` to pend.
    struct PendingReconnectClient {
        inner: MockBleClient,
    }

    #[async_trait]
    impl BleClient for PendingReconnectClient {
        async fn connect(&mut self) -> Result<()> {
            self.inner.connect().await
        }

        async fn write_state(&self, state: bool) -> Result<()> {
            self.inner.write_state(state).await
        }

        async fn read_state(&self) -> Result<u8> {
            self.inner.read_state().await
        }

        async fn disconnect(&mut self) -> Result<()> {
            self.inner.disconnect().await
        }

        async fn wait_for_reconnect(&self) -> Result<()> {
            std::future::pending().await
        }
    }

    fn pending(client: MockBleClient) -> Box<dyn BleClient> {
        Box::new(PendingReconnectClient { inner: client })
    }

    /// Hand-written fake for the reconnect path: records every write, and makes
    /// `wait_for_reconnect` resolve exactly once (the first call) so the loop
    /// sees a single reconnect before `wait_for_edge` terminates the run.
    struct FakeReconnectClient {
        writes: Arc<Mutex<Vec<bool>>>,
        reconnect_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl BleClient for FakeReconnectClient {
        async fn connect(&mut self) -> Result<()> {
            Ok(())
        }

        async fn write_state(&self, state: bool) -> Result<()> {
            self.writes.lock().unwrap().push(state);
            Ok(())
        }

        async fn read_state(&self) -> Result<u8> {
            Ok(0)
        }

        async fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }

        async fn wait_for_reconnect(&self) -> Result<()> {
            if self.reconnect_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                std::future::pending().await
            }
        }
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
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
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
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn instrumented_edge_reads_once_and_logs() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

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
        client.expect_read_state().times(1).returning(|| Ok(0));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(Some("1")),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn read_error_does_not_abort_run() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

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
        client
            .expect_read_state()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("read failed")));
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(Some("1")),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test]
    async fn disabled_edge_never_reads() {
        let mut client = MockBleClient::new();
        let mut gpio = MockGpioReader::new();

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
        // No `expect_read_state`: any read would panic and fail the run.
        gpio.expect_read_level()
            .times(1)
            .returning(|| Ok(Level::Low));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Ok(edge(Level::High)));
        gpio.expect_wait_for_edge()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("stop")));

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
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
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
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

    /// Pend-capable fake reader for the keepalive tests. `wait_for_edge` stays
    /// pending until a fixed deadline measured from construction, then errors.
    /// The deadline is absolute so that `select!` cancelling the pending future
    /// on a keepalive tick does not restart the wait.
    struct FakePendingReader {
        level: Level,
        deadline: tokio::time::Instant,
    }

    impl FakePendingReader {
        fn new(level: Level, stop_after: Duration) -> Self {
            Self {
                level,
                deadline: tokio::time::Instant::now() + stop_after,
            }
        }
    }

    #[async_trait]
    impl GpioReader for FakePendingReader {
        async fn wait_for_edge(&mut self) -> Result<GpioEdge> {
            tokio::time::sleep_until(self.deadline).await;
            Err(anyhow::anyhow!("stop"))
        }

        fn read_level(&mut self) -> Result<Level> {
            Ok(self.level)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_writes_current_fact_every_10s_while_idle() {
        let mut client = MockBleClient::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(4)
            .returning(|_| Ok(()));

        // Stop off the 10 s tick boundary so the unbiased `select!` cannot race
        // the keepalive tick against the terminal edge error.
        let gpio = FakePendingReader::new(Level::Low, Duration::from_secs(35));

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test(start_paused = true)]
    async fn no_write_before_keepalive_window() {
        let mut client = MockBleClient::new();

        client.expect_connect().times(1).returning(|| Ok(()));
        client
            .expect_write_state()
            .with(eq(true))
            .times(1)
            .returning(|_| Ok(()));

        let gpio = FakePendingReader::new(Level::Low, Duration::from_secs(5));

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_resyncs_current_fact_once() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let reconnect_calls = Arc::new(AtomicUsize::new(0));
        let client = FakeReconnectClient {
            writes: Arc::clone(&writes),
            reconnect_calls: Arc::clone(&reconnect_calls),
        };

        // The first `wait_for_edge` stays pending for 50 ms, so the immediately
        // ready first `wait_for_reconnect` wins and the run ends on the edge.
        let gpio = FakePendingReader::new(Level::Low, Duration::from_millis(50));

        let res = run_loop(
            Box::new(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;

        assert_eq!(res.unwrap_err().to_string(), "stop");
        assert_eq!(*writes.lock().unwrap(), vec![true, true]);
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
            pending(client),
            Box::new(gpio),
            Box::new(passing_interval()),
            crate::instrumentation::Instrumentation::from_value(None),
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

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(interval),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
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

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(interval),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
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

        let res = run_loop(
            pending(client),
            Box::new(gpio),
            Box::new(interval),
            crate::instrumentation::Instrumentation::from_value(None),
        )
        .await;
        assert_eq!(res.unwrap_err().to_string(), "stop");
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_resyncs_on_connect() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Connected, None, None, true, t0),
            WriteDecision::Write(true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_resync_wins_over_keepalive() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(
                ConnectionEvent::Connected,
                Some(false),
                Some(t0),
                false,
                t0 + KEEPALIVE * 10,
            ),
            WriteDecision::Write(false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_writes_on_change() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Steady, Some(false), Some(t0), true, t0),
            WriteDecision::Write(true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_writes_on_change_down() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Steady, Some(true), Some(t0), false, t0),
            WriteDecision::Write(false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_idle_when_unchanged() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Steady, Some(true), Some(t0), true, t0),
            WriteDecision::Idle
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_idle_when_unchanged_false() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Steady, Some(false), Some(t0), false, t0),
            WriteDecision::Idle
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_keepalive_when_due() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(
                ConnectionEvent::Steady,
                Some(true),
                Some(t0),
                true,
                t0 + KEEPALIVE,
            ),
            WriteDecision::Write(true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_change_beats_coincident_keepalive() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(
                ConnectionEvent::Steady,
                Some(false),
                Some(t0),
                true,
                t0 + KEEPALIVE,
            ),
            WriteDecision::Write(true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_write_steady_without_last_write_is_keepalive_due() {
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            schedule_write(ConnectionEvent::Steady, Some(true), None, true, t0),
            WriteDecision::Write(true)
        );
    }
}
