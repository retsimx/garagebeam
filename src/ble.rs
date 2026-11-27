use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::{
    Central, CentralEvent, CharPropFlags, Characteristic, Manager as _, Peripheral as _,
    ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, watch};

pub const SERVICE_UUID: &str = "6a4c0001-b5a3-4f1e-9c2d-7e8f9a0b1c2d";
pub const CHARACTERISTIC_UUID: &str = "6a4c0002-b5a3-4f1e-9c2d-7e8f9a0b1c2d";
pub const FACT_INTACT: u8 = 0x00;
pub const FACT_BROKEN: u8 = 0x01;
pub const CONNECT_BOUND: Duration = Duration::from_secs(30);
pub const KEEPALIVE_TIMEOUT_MS: u64 = 3000;
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(KEEPALIVE_TIMEOUT_MS);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX_SHIFT: u32 = 3;
/// Consecutive failed acquisitions (a scan window that produced no connection)
/// before the supervisor tears down and rebuilds the BlueZ session. A stalled
/// adapter can stop delivering discovery events indefinitely, so rebuilding the
/// `Manager`, adapter, and event stream is the only in-process recovery.
const MAX_ATTEMPTS_BEFORE_REINIT: u32 = 5;

type CentralEventStream = Pin<Box<dyn Stream<Item = CentralEvent> + Send>>;

#[cfg(test)]
use mockall::{automock, predicate::*};

#[cfg_attr(test, automock)]
#[async_trait]
pub trait BleClient: Send + Sync {
    async fn connect(&mut self) -> Result<()>;
    async fn write_state(&self, state: bool, ack: bool) -> Result<()>;
    /// Measurement-only read of the 1-byte fact characteristic. This is not
    /// part of the control path: it exists solely to sample write-to-read
    /// latency and must never gate or alter a write decision.
    async fn read_state(&self) -> Result<u8>;
    /// Resolves the next time a fresh connection is established after the one
    /// `connect()` returned. Never resolves for the already-observed link.
    async fn wait_for_reconnect(&self) -> Result<()>;
    #[allow(dead_code)]
    async fn disconnect(&mut self) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectDecision {
    Connect,
    Wait,
    GiveUp,
}

pub fn connect_decision(configured: &str, candidate: &str, elapsed: Duration) -> ConnectDecision {
    if elapsed >= CONNECT_BOUND {
        ConnectDecision::GiveUp
    } else if candidate.eq_ignore_ascii_case(configured) {
        ConnectDecision::Connect
    } else {
        ConnectDecision::Wait
    }
}

pub fn backoff_delay(attempt: u32) -> Duration {
    BACKOFF_BASE * (1u32 << attempt.min(BACKOFF_MAX_SHIFT))
}

/// Whether `failed_attempts` consecutive failed acquisitions should trigger a
/// full BlueZ session rebuild (manager, adapter, and event stream) instead of
/// another backoff-and-rescan on the same adapter.
pub fn should_reinit(failed_attempts: u32) -> bool {
    failed_attempts >= MAX_ATTEMPTS_BEFORE_REINIT
}

pub fn fact_byte(state: bool) -> u8 {
    if state {
        FACT_BROKEN
    } else {
        FACT_INTACT
    }
}

/// Select the ATT write mode for an acknowledged or non-acknowledged write.
/// Pre-v12 peripherals without the WRITE property remain usable through the
/// WriteWithoutResponse compatibility path.
pub fn select_write_type(properties: CharPropFlags, ack: bool) -> (WriteType, bool) {
    if !ack {
        (WriteType::WithoutResponse, false)
    } else if properties.contains(CharPropFlags::WRITE) {
        (WriteType::WithResponse, false)
    } else {
        (WriteType::WithoutResponse, true)
    }
}

pub async fn bounded_write<T, F>(future: F) -> std::result::Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(WRITE_TIMEOUT, future).await
}

fn clear_connection_and_signal_reconnect<T>(
    connection: &Arc<Mutex<Option<T>>>,
    reconnect_tx: &mpsc::UnboundedSender<()>,
) {
    *connection.lock().unwrap() = None;
    let _ = reconnect_tx.send(());
}

async fn bounded_disconnect<F>(future: F) -> std::result::Result<(), tokio::time::error::Elapsed>
where
    F: Future<Output = ()>,
{
    tokio::time::timeout(WRITE_TIMEOUT, future).await
}

fn start_write_failure_teardown<T, D, DF>(
    connection: &Arc<Mutex<Option<T>>>,
    reconnect_tx: &mpsc::UnboundedSender<()>,
    disconnect: D,
) where
    D: FnOnce() -> DF + Send + 'static,
    DF: Future<Output = ()> + Send + 'static,
{
    // Drop the shared target and wake the supervisor before starting any
    // potentially blocking peripheral teardown.
    clear_connection_and_signal_reconnect(connection, reconnect_tx);

    let disconnect_future = disconnect();
    tokio::spawn(async move {
        match bounded_disconnect(disconnect_future).await {
            Ok(()) => {}
            Err(_) => tracing::warn!(
                timeout_ms = WRITE_TIMEOUT.as_millis() as u64,
                "BLE peripheral disconnect timed out"
            ),
        }
    });
}

async fn write_state_with_teardown<W, E, D, DF, T>(
    write_future: W,
    disconnect: D,
    connection: &Arc<Mutex<Option<T>>>,
    reconnect_tx: &mpsc::UnboundedSender<()>,
    write_type: WriteType,
) -> Result<()>
where
    W: Future<Output = std::result::Result<(), E>>,
    E: Into<anyhow::Error> + Send + Sync + 'static,
    D: FnOnce() -> DF + Send + 'static,
    DF: Future<Output = ()> + Send + 'static,
{
    match bounded_write(write_future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            let e: anyhow::Error = e.into();
            tracing::warn!(
                write_type = ?write_type,
                error = %e,
                "BLE state write failed"
            );
            start_write_failure_teardown(connection, reconnect_tx, disconnect);
            Err(e).context("Failed to write to state characteristic")
        }
        Err(_) => {
            tracing::warn!(
                write_type = ?write_type,
                timeout_ms = WRITE_TIMEOUT.as_millis() as u64,
                "BLE state write timed out"
            );
            start_write_failure_teardown(connection, reconnect_tx, disconnect);
            Err(anyhow!(
                "state characteristic write timed out after {} ms",
                WRITE_TIMEOUT.as_millis()
            ))
        }
    }
}

struct Connection {
    peripheral: Peripheral,
    characteristic: Characteristic,
}

#[derive(Clone)]
enum LinkStatus {
    Waiting,
    Connected,
    Fatal(String),
}

/// Connection-generation state machine shared by the supervisor and the client.
///
/// The supervisor calls [`advance`](Self::advance) immediately before it
/// publishes [`LinkStatus::Connected`]; `connect()` calls
/// [`mark_seen`](Self::mark_seen) once it observes that link, so the epoch of
/// the already-observed link is never reported as a reconnect. Each `advance`
/// after that resolves exactly one [`wait`](Self::wait).
struct ConnectionEpoch {
    tx: watch::Sender<u64>,
    rx: watch::Receiver<u64>,
    last_seen: AtomicU64,
}

impl ConnectionEpoch {
    fn new() -> Self {
        let (tx, rx) = watch::channel(0u64);
        Self {
            tx,
            rx,
            last_seen: AtomicU64::new(0),
        }
    }

    /// Announce a fresh connection. Call BEFORE publishing `Connected`.
    fn advance(&self) {
        self.tx.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Mark the link just observed, so it is not reported as a reconnect.
    fn mark_seen(&self) {
        self.last_seen.store(*self.rx.borrow(), Ordering::SeqCst);
    }

    /// Resolves once the epoch advances past the last observed value.
    async fn wait(&self) -> Result<()> {
        let mut rx = self.rx.clone();
        loop {
            let current = *rx.borrow();
            if current != self.last_seen.load(Ordering::SeqCst) {
                self.last_seen.store(current, Ordering::SeqCst);
                return Ok(());
            }
            rx.changed()
                .await
                .context("connection epoch channel closed")?;
        }
    }
}

#[derive(Default)]
pub struct AdapterSlot {
    adapter: Option<Adapter>,
    scan_active: bool,
}

impl AdapterSlot {
    fn clear(&mut self) {
        self.adapter = None;
        self.scan_active = false;
    }

    fn is_scan_active(&self) -> bool {
        self.scan_active
    }

    fn set_scan_active(&mut self, active: bool) {
        self.scan_active = active;
    }
}

pub type SharedAdapter = Arc<Mutex<AdapterSlot>>;

fn lock_slot(shared: &SharedAdapter) -> std::sync::MutexGuard<'_, AdapterSlot> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct BtleplugClient {
    mac_address: String,
    connection: Arc<Mutex<Option<Connection>>>,
    shared_adapter: SharedAdapter,
    reconnect_tx: mpsc::UnboundedSender<()>,
    status_tx: watch::Sender<LinkStatus>,
    status_rx: watch::Receiver<LinkStatus>,
    epoch: Arc<ConnectionEpoch>,
    reconnect_rx: Option<mpsc::UnboundedReceiver<()>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    ack_fallback_warned: AtomicBool,
}

impl BtleplugClient {
    pub fn new(mac_address: String, shared_adapter: SharedAdapter) -> Self {
        let (status_tx, status_rx) = watch::channel(LinkStatus::Waiting);
        let (reconnect_tx, reconnect_rx) = mpsc::unbounded_channel();
        Self {
            mac_address,
            connection: Arc::new(Mutex::new(None)),
            shared_adapter,
            reconnect_tx,
            status_tx,
            status_rx,
            epoch: Arc::new(ConnectionEpoch::new()),
            reconnect_rx: Some(reconnect_rx),
            supervisor: None,
            ack_fallback_warned: AtomicBool::new(false),
        }
    }
}

async fn connect_peripheral(peripheral: &Peripheral) -> Result<Characteristic> {
    peripheral.connect().await.context("Failed to connect")?;
    peripheral
        .discover_services()
        .await
        .context("Failed to discover services")?;
    let characteristic_uuid =
        uuid::Uuid::parse_str(CHARACTERISTIC_UUID).expect("valid characteristic UUID");
    peripheral
        .characteristics()
        .into_iter()
        .find(|characteristic| characteristic.uuid == characteristic_uuid)
        .ok_or_else(|| anyhow!("Characteristic {CHARACTERISTIC_UUID} not found"))
}

/// Acquire the BlueZ adapter and its event stream, reusing a cached adapter
/// across supervisor reinits and `run_loop` restarts so we do not open a new
/// `Manager` / D-Bus session unless needed. If a cached adapter fails to
/// produce an event stream, clear the slot so the next attempt rebuilds cleanly.
async fn setup_central(shared_adapter: &SharedAdapter) -> Result<(Adapter, CentralEventStream)> {
    let cached = lock_slot(shared_adapter).adapter.clone();
    if let Some(central) = cached {
        return match central.events().await {
            Ok(events) => Ok((central, events)),
            Err(e) => {
                lock_slot(shared_adapter).clear();
                Err(e).context("failed to subscribe to adapter events")
            }
        };
    }

    let manager = Manager::new()
        .await
        .context("failed to create BlueZ manager")?;
    let central = manager
        .adapters()
        .await
        .context("failed to enumerate adapters")?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No Bluetooth adapter found"))?;
    // Subscribe before any scan: BlueZ synthesises discovery events for known
    // peripherals and replays them on a fresh subscription.
    let events = central
        .events()
        .await
        .context("failed to subscribe to adapter events")?;
    lock_slot(shared_adapter).adapter = Some(central.clone());
    Ok((central, events))
}

async fn stop_scan_if_active(central: &Adapter, shared_adapter: &SharedAdapter) {
    if !lock_slot(shared_adapter).is_scan_active() {
        return;
    }
    if let Err(e) = central.stop_scan().await {
        tracing::warn!(error = %e, "failed to stop BLE scan");
    }
    lock_slot(shared_adapter).set_scan_active(false);
}

async fn supervise(
    mac_address: String,
    connection: Arc<Mutex<Option<Connection>>>,
    shared_adapter: SharedAdapter,
    status_tx: watch::Sender<LinkStatus>,
    epoch: Arc<ConnectionEpoch>,
    mut reconnect_rx: mpsc::UnboundedReceiver<()>,
) {
    let service_uuid = uuid::Uuid::parse_str(SERVICE_UUID).expect("valid service UUID");
    let filter = ScanFilter {
        services: vec![service_uuid],
    };

    let mut attempt: u32 = 0;

    'reinit: loop {
        let (central, mut events) = match setup_central(&shared_adapter).await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = status_tx.send_replace(LinkStatus::Fatal(e.to_string()));
                let delay = backoff_delay(attempt);
                attempt = attempt.saturating_add(1);
                tracing::info!(
                    retry_in_ms = delay.as_millis() as u64,
                    error = %e,
                    "ble supervisor setup failed; backing off"
                );
                tokio::time::sleep(delay).await;
                continue 'reinit;
            }
        };
        // A fresh adapter and event stream start the acquisition run clean.
        attempt = 0;

        loop {
            let started = std::time::Instant::now();
            let deadline = tokio::time::Instant::now() + CONNECT_BOUND;
            let mut stream_ended = false;

            stop_scan_if_active(&central, &shared_adapter).await;
            lock_slot(&shared_adapter).set_scan_active(true);

            let acquired = match central.start_scan(filter.clone()).await {
                Ok(()) => {
                    let mut acquired = None;
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break,
                            event = events.next() => match event {
                                Some(CentralEvent::DeviceDiscovered(id)) => {
                                    let peripheral = match central.peripheral(&id).await {
                                        Ok(peripheral) => peripheral,
                                        Err(_) => continue,
                                    };
                                    let address = peripheral.address().to_string();
                                    let elapsed = started.elapsed();
                                    match connect_decision(&mac_address, &address, elapsed) {
                                        ConnectDecision::Connect => {
                                            tracing::info!(
                                                address = %address,
                                                elapsed_ms = elapsed.as_millis() as u64,
                                                "connecting to discovered peripheral"
                                            );
                                            stop_scan_if_active(&central, &shared_adapter).await;
                                            match connect_peripheral(&peripheral).await {
                                                Ok(characteristic) => {
                                                    acquired = Some((
                                                        Connection {
                                                            peripheral,
                                                            characteristic,
                                                        },
                                                        id,
                                                    ));
                                                    break;
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                        address = %address,
                                                        error = %e,
                                                        "connect attempt failed"
                                                    );
                                                    let _ = peripheral.disconnect().await;
                                                    break;
                                                }
                                            }
                                        }
                                        ConnectDecision::Wait => continue,
                                        ConnectDecision::GiveUp => break,
                                    }
                                }
                                Some(_) => continue,
                                None => {
                                    stream_ended = true;
                                    break;
                                }
                            }
                        }
                    }
                    acquired
                }
                Err(e) => {
                    lock_slot(&shared_adapter).set_scan_active(false);
                    tracing::warn!(error = %e, "failed to start scan");
                    None
                }
            };

            stop_scan_if_active(&central, &shared_adapter).await;

            if stream_ended {
                lock_slot(&shared_adapter).clear();
                let _ = status_tx
                    .send_replace(LinkStatus::Fatal("adapter event stream ended".to_string()));
                attempt = 0;
                continue 'reinit;
            }

            let (connection_state, our_id) = match acquired {
                Some(connection_state) => connection_state,
                None => {
                    let delay = backoff_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    tracing::info!(
                        retry_in_ms = delay.as_millis() as u64,
                        "scan attempt failed; backing off"
                    );
                    tokio::time::sleep(delay).await;
                    if should_reinit(attempt) {
                        tracing::info!("ble_supervisor_reinit");
                        lock_slot(&shared_adapter).clear();
                        attempt = 0;
                        continue 'reinit;
                    }
                    continue;
                }
            };

            attempt = 0;
            *connection.lock().unwrap() = Some(connection_state);
            epoch.advance();
            let _ = status_tx.send_replace(LinkStatus::Connected);
            // A write failure while the supervisor was still acquiring buffered a
            // reconnect signal; drop it before entering Hold so it cannot tear down
            // this fresh, healthy link.
            while reconnect_rx.try_recv().is_ok() {}

            loop {
                tokio::select! {
                    event = events.next() => match event {
                        Some(CentralEvent::DeviceDisconnected(id)) if id == our_id => break,
                        Some(_) => continue,
                        None => {
                            stream_ended = true;
                            break;
                        }
                    },
                    Some(_) = reconnect_rx.recv() => break,
                }
            }

            if stream_ended {
                lock_slot(&shared_adapter).clear();
            }

            let connection_state = connection.lock().unwrap().take();
            if let Some(connection_state) = connection_state {
                let _ = connection_state.peripheral.disconnect().await;
            }

            if stream_ended {
                let _ = status_tx
                    .send_replace(LinkStatus::Fatal("adapter event stream ended".to_string()));
                continue 'reinit;
            }
            let _ = status_tx.send_replace(LinkStatus::Waiting);
        }
    }
}

#[async_trait]
impl BleClient for BtleplugClient {
    async fn connect(&mut self) -> Result<()> {
        if self.supervisor.is_none() {
            // `disconnect()` aborts the supervisor and consumes the receiver.
            // Re-arm a fresh channel so a later `connect()` cannot panic on a
            // missing receiver.
            if self.reconnect_rx.is_none() {
                let (reconnect_tx, reconnect_rx) = mpsc::unbounded_channel();
                self.reconnect_tx = reconnect_tx;
                self.reconnect_rx = Some(reconnect_rx);
            }
            let mac_address = self.mac_address.clone();
            let connection = Arc::clone(&self.connection);
            let shared_adapter = Arc::clone(&self.shared_adapter);
            let status_tx = self.status_tx.clone();
            let epoch = Arc::clone(&self.epoch);
            let reconnect_rx = self
                .reconnect_rx
                .take()
                .ok_or_else(|| anyhow!("reconnect receiver unavailable before supervisor start"))?;
            self.supervisor = Some(tokio::spawn(supervise(
                mac_address,
                connection,
                shared_adapter,
                status_tx,
                epoch,
                reconnect_rx,
            )));
        }

        loop {
            // `borrow` leaves the seen version untouched, so a status sent
            // between this check and `changed` still wakes us.
            let status = self.status_rx.borrow().clone();
            match status {
                LinkStatus::Connected => {
                    self.epoch.mark_seen();
                    return Ok(());
                }
                LinkStatus::Fatal(message) => return Err(anyhow!(message)),
                LinkStatus::Waiting => {
                    self.status_rx
                        .changed()
                        .await
                        .context("connect status channel closed")?;
                }
            }
        }
    }

    async fn write_state(&self, state: bool, ack: bool) -> Result<()> {
        let target = {
            let connection = self.connection.lock().unwrap();
            connection.as_ref().map(|connection| {
                (
                    connection.peripheral.clone(),
                    connection.characteristic.clone(),
                )
            })
        };
        let (peripheral, characteristic) = target.ok_or_else(|| anyhow!("not connected"))?;

        let (write_type, property_fallback) = select_write_type(characteristic.properties, ack);
        if property_fallback && !self.ack_fallback_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                characteristic = CHARACTERISTIC_UUID,
                "ACK requested but characteristic lacks WRITE property; using WriteWithoutResponse compatibility fallback"
            );
        }

        let byte = fact_byte(state);
        let peripheral_for_disconnect = peripheral.clone();
        write_state_with_teardown(
            peripheral.write(&characteristic, &[byte], write_type),
            move || async move {
                let _ = peripheral_for_disconnect.disconnect().await;
            },
            &self.connection,
            &self.reconnect_tx,
            write_type,
        )
        .await
    }

    async fn read_state(&self) -> Result<u8> {
        let target = {
            let connection = self.connection.lock().unwrap();
            connection.as_ref().map(|connection| {
                (
                    connection.peripheral.clone(),
                    connection.characteristic.clone(),
                )
            })
        };
        let (peripheral, characteristic) = target.ok_or_else(|| anyhow!("not connected"))?;

        let value = peripheral
            .read(&characteristic)
            .await
            .context("Failed to read state characteristic")?;
        value
            .first()
            .copied()
            .ok_or_else(|| anyhow!("state characteristic returned no bytes"))
    }

    async fn wait_for_reconnect(&self) -> Result<()> {
        self.epoch.wait().await
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.abort();
        }
        let connection = self.connection.lock().unwrap().take();
        if let Some(connection) = connection {
            let _ = connection.peripheral.disconnect().await;
        }
        let _ = self.status_tx.send_replace(LinkStatus::Waiting);
        Ok(())
    }
}

impl Drop for BtleplugClient {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fact_byte_maps_both_states() {
        assert_eq!(fact_byte(false), FACT_INTACT);
        assert_eq!(fact_byte(true), FACT_BROKEN);
        assert_eq!(FACT_INTACT, 0x00);
        assert_eq!(FACT_BROKEN, 0x01);
    }

    #[test]
    fn ack_selects_with_response_when_write_property_exists() {
        let (write_type, fallback) = select_write_type(CharPropFlags::WRITE, true);
        assert_eq!(write_type, WriteType::WithResponse);
        assert!(!fallback);
    }

    #[test]
    fn no_ack_always_selects_without_response() {
        let (write_type, fallback) = select_write_type(
            CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE,
            false,
        );
        assert_eq!(write_type, WriteType::WithoutResponse);
        assert!(!fallback);
    }

    #[test]
    fn ack_falls_back_once_when_write_property_is_absent() {
        let (write_type, fallback) = select_write_type(
            CharPropFlags::READ | CharPropFlags::WRITE_WITHOUT_RESPONSE,
            true,
        );
        assert_eq!(write_type, WriteType::WithoutResponse);
        assert!(fallback);
    }

    #[test]
    fn write_timeout_is_three_seconds() {
        assert_eq!(WRITE_TIMEOUT, Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn write_completes_before_three_second_boundary() {
        let result = bounded_write(async {
            tokio::time::sleep(Duration::from_millis(2999)).await;
            42
        })
        .await;
        assert_eq!(result, Ok(42));
    }

    #[tokio::test(start_paused = true)]
    async fn write_times_out_after_three_second_boundary() {
        let result = bounded_write(async {
            tokio::time::sleep(Duration::from_millis(3001)).await;
            42
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_is_bounded_by_three_seconds() {
        let started = tokio::time::Instant::now();
        let result = bounded_disconnect(async {
            std::future::pending::<()>().await;
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            WRITE_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn write_timeout_clears_signals_and_does_not_wait_for_stalled_disconnect() {
        let connection = Arc::new(Mutex::new(Some(42u8)));
        let connection_for_disconnect = Arc::clone(&connection);
        let (reconnect_tx, mut reconnect_rx) = mpsc::unbounded_channel();
        let clear_seen_before_disconnect = Arc::new(AtomicBool::new(false));
        let clear_seen_for_disconnect = Arc::clone(&clear_seen_before_disconnect);
        let disconnect_started = Arc::new(AtomicBool::new(false));
        let disconnect_started_for_disconnect = Arc::clone(&disconnect_started);

        let started = tokio::time::Instant::now();
        let result = write_state_with_teardown(
            async { std::future::pending::<std::result::Result<(), anyhow::Error>>().await },
            move || {
                clear_seen_for_disconnect.store(
                    connection_for_disconnect.lock().unwrap().is_none(),
                    Ordering::SeqCst,
                );
                disconnect_started_for_disconnect.store(true, Ordering::SeqCst);
                async move {
                    std::future::pending::<()>().await;
                }
            },
            &connection,
            &reconnect_tx,
            WriteType::WithoutResponse,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            WRITE_TIMEOUT
        );
        assert!(clear_seen_before_disconnect.load(Ordering::SeqCst));
        assert!(disconnect_started.load(Ordering::SeqCst));
        assert_eq!(*connection.lock().unwrap(), None);
        assert!(reconnect_rx.try_recv().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn write_error_clears_signals_and_does_not_wait_for_stalled_disconnect() {
        let connection = Arc::new(Mutex::new(Some(42u8)));
        let (reconnect_tx, mut reconnect_rx) = mpsc::unbounded_channel();
        let disconnect_started = Arc::new(AtomicBool::new(false));
        let disconnect_started_for_disconnect = Arc::clone(&disconnect_started);

        let started = tokio::time::Instant::now();
        let result = write_state_with_teardown(
            async { Err(anyhow!("write failed")) },
            move || {
                disconnect_started_for_disconnect.store(true, Ordering::SeqCst);
                async move {
                    std::future::pending::<()>().await;
                }
            },
            &connection,
            &reconnect_tx,
            WriteType::WithResponse,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            Duration::ZERO
        );
        assert!(disconnect_started.load(Ordering::SeqCst));
        assert_eq!(*connection.lock().unwrap(), None);
        assert!(reconnect_rx.try_recv().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn write_success_keeps_connection_and_does_not_start_disconnect() {
        let connection = Arc::new(Mutex::new(Some(42u8)));
        let (reconnect_tx, mut reconnect_rx) = mpsc::unbounded_channel();
        let disconnect_started = Arc::new(AtomicBool::new(false));
        let disconnect_started_for_disconnect = Arc::clone(&disconnect_started);

        let result = write_state_with_teardown(
            async { Ok::<(), anyhow::Error>(()) },
            move || {
                disconnect_started_for_disconnect.store(true, Ordering::SeqCst);
                async move {}
            },
            &connection,
            &reconnect_tx,
            WriteType::WithoutResponse,
        )
        .await;

        assert!(result.is_ok());
        assert!(!disconnect_started.load(Ordering::SeqCst));
        assert_eq!(*connection.lock().unwrap(), Some(42u8));
        assert!(reconnect_rx.try_recv().is_err());
    }

    #[test]
    fn write_failure_clears_connection_and_signals_reconnect() {
        let connection = Arc::new(Mutex::new(Some(42u8)));
        let (reconnect_tx, mut reconnect_rx) = mpsc::unbounded_channel();

        clear_connection_and_signal_reconnect(&connection, &reconnect_tx);

        assert_eq!(*connection.lock().unwrap(), None);
        assert!(reconnect_rx.try_recv().is_ok());
    }

    #[test]
    fn connects_on_matching_address_within_bound() {
        assert_eq!(
            connect_decision(
                "AA:BB:CC:DD:EE:FF",
                "AA:BB:CC:DD:EE:FF",
                Duration::from_secs(1)
            ),
            ConnectDecision::Connect
        );
    }

    #[test]
    fn connect_match_is_case_insensitive() {
        assert_eq!(
            connect_decision(
                "aa:bb:cc:dd:ee:ff",
                "AA:BB:CC:DD:EE:FF",
                Duration::from_secs(1)
            ),
            ConnectDecision::Connect
        );
    }

    #[test]
    fn waits_for_non_matching_address_within_bound() {
        assert_eq!(
            connect_decision(
                "AA:BB:CC:DD:EE:FF",
                "11:22:33:44:55:66",
                Duration::from_secs(1)
            ),
            ConnectDecision::Wait
        );
    }

    #[test]
    fn gives_up_at_and_beyond_bound() {
        assert_eq!(
            connect_decision("AA:BB:CC:DD:EE:FF", "AA:BB:CC:DD:EE:FF", CONNECT_BOUND),
            ConnectDecision::GiveUp
        );
        assert_eq!(
            connect_decision(
                "AA:BB:CC:DD:EE:FF",
                "11:22:33:44:55:66",
                CONNECT_BOUND + Duration::from_secs(1)
            ),
            ConnectDecision::GiveUp
        );
    }

    #[test]
    fn backoff_delay_is_strictly_positive() {
        for attempt in 0..=10 {
            assert!(backoff_delay(attempt) > Duration::ZERO);
        }
    }

    #[test]
    fn backoff_delay_is_monotonic_and_capped() {
        let mut previous = backoff_delay(0);
        for attempt in 1..=10 {
            let current = backoff_delay(attempt);
            assert!(current >= previous);
            previous = current;
        }
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(10), Duration::from_secs(8));
    }

    #[test]
    fn reinit_not_triggered_below_bound() {
        for failed in 0..MAX_ATTEMPTS_BEFORE_REINIT {
            assert!(!should_reinit(failed), "unexpected reinit at {failed}");
        }
    }

    #[test]
    fn reinit_triggered_at_and_above_bound() {
        assert!(should_reinit(MAX_ATTEMPTS_BEFORE_REINIT));
        assert!(should_reinit(MAX_ATTEMPTS_BEFORE_REINIT + 1));
        assert_eq!(MAX_ATTEMPTS_BEFORE_REINIT, 5);
    }

    #[test]
    fn backoff_sequence_before_reinit_is_capped() {
        let sequence: Vec<Duration> = (0..MAX_ATTEMPTS_BEFORE_REINIT).map(backoff_delay).collect();
        assert_eq!(
            sequence,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(8),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_first_connect_does_not_report_reconnect() {
        let epoch = ConnectionEpoch::new();
        epoch.advance();
        epoch.mark_seen();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), epoch.wait())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_one_increment_resolves_exactly_once() {
        let epoch = ConnectionEpoch::new();
        epoch.advance();
        epoch.mark_seen();
        epoch.advance();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), epoch.wait())
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), epoch.wait())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_no_advance_does_not_resolve() {
        let epoch = ConnectionEpoch::new();
        epoch.advance();
        epoch.mark_seen();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), epoch.wait())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_unmarked_increment_resolves_once() {
        let epoch = ConnectionEpoch::new();
        epoch.advance();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), epoch.wait())
                .await
                .is_ok()
        );
    }

    #[test]
    fn adapter_slot_defaults_to_empty_and_inactive() {
        let slot = AdapterSlot::default();
        assert!(slot.adapter.is_none());
        assert!(!slot.is_scan_active());
    }

    #[test]
    fn adapter_slot_tracks_scan_active_and_clear_resets_state() {
        let mut slot = AdapterSlot::default();
        assert!(!slot.is_scan_active());

        slot.set_scan_active(true);
        assert!(slot.is_scan_active());

        slot.set_scan_active(false);
        assert!(!slot.is_scan_active());

        slot.set_scan_active(true);
        slot.clear();
        assert!(slot.adapter.is_none());
        assert!(!slot.is_scan_active());
    }

    #[test]
    fn lock_slot_recovers_from_poisoned_mutex() {
        let shared = SharedAdapter::default();
        let shared_for_panic = Arc::clone(&shared);
        let result = std::thread::spawn(move || {
            let mut guard = lock_slot(&shared_for_panic);
            guard.set_scan_active(true);
            panic!("simulated panic while holding AdapterSlot lock");
        })
        .join();
        assert!(result.is_err());

        let mut recovered = lock_slot(&shared);
        assert!(recovered.is_scan_active());
        recovered.clear();
        assert!(!recovered.is_scan_active());
        assert!(recovered.adapter.is_none());
    }
}
