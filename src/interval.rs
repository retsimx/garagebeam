use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[cfg(test)]
use mockall::automock;

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_HCI: i32 = 1;
const HCI_DEV: u16 = 0;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_RAW: u16 = 0;
// HCI channel numbers are kernel-specific. On the Raspberry Pi 6.12.y kernel,
// `include/net/bluetooth/hci_sock.h` defines RAW=0, USER=1, MONITOR=2,
// CONTROL=3, LOGGING=4; mainline uses MONITOR=3. This value was verified against
// the deployed Pi kernel source on 2026-09-19. The monitor socket must be a
// separate socket from the raw command channel (0).
const HCI_CHANNEL_MONITOR: u16 = 2;

const SOL_HCI: i32 = 0;
const HCI_FILTER: i32 = 2;
const HCI_EVENT_PKT: u32 = 0x04;

const HCI_EVENT_COMMAND_COMPLETE: u8 = 0x0e;
const HCI_EVENT_COMMAND_STATUS: u8 = 0x0f;
const HCI_EVENT_LE_META: u8 = 0x3e;
const LE_SUBEVENT_CONNECTION_UPDATE_COMPLETE: u8 = 0x03;

// OGF 0x08 (LE Controller), OCF 0x0013.
const HCI_LE_CONN_UPDATE: u16 = 0x2013;

#[cfg(target_pointer_width = "64")]
const HCIGETCONNLIST: libc::c_ulong = 0x800448d4;
#[cfg(target_pointer_width = "32")]
const HCIGETCONNLIST: libc::c_int = 0x800448d4u32 as libc::c_int;

const HCI_MON_HDR_SIZE: usize = 6;
const HCI_MON_EVENT_PKT: u16 = 3;

const HCI_READ_TIMEOUT: Duration = Duration::from_secs(2);
const CONN_UPDATE_TIMEOUT: Duration = Duration::from_secs(15);
const GUARD_POLL_TIMEOUT_MS: i32 = 500;

pub const TARGET_INTERVAL: u16 = 0x0006; // 7.5 ms in 1.25 ms units
pub const SLAVE_LATENCY: u16 = 0;
pub const SUPERVISION_TIMEOUT: u16 = 0x0064; // 1000 ms in 10 ms units
pub const REAPPLY_RATE_LIMIT: Duration = Duration::from_secs(3);

// The kernel `hci_cp_le_conn_update` is 14 bytes; the trailing CE-length fields
// are required, and a 10-byte parameter block is rejected with EINVAL.
const MIN_CE_LEN: u16 = 0x0001;
const MAX_CE_LEN: u16 = 0x0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalDecision {
    Apply,
    Skip,
}

pub fn should_reapply(
    reported: u16,
    last_attempt: Option<Instant>,
    now: Instant,
) -> IntervalDecision {
    if reported == TARGET_INTERVAL {
        return IntervalDecision::Skip;
    }
    match last_attempt {
        None => IntervalDecision::Apply,
        Some(attempted) if now.saturating_duration_since(attempted) >= REAPPLY_RATE_LIMIT => {
            IntervalDecision::Apply
        }
        Some(_) => IntervalDecision::Skip,
    }
}

pub fn conn_update_params(handle: u16) -> [u8; 14] {
    let mut params = [0u8; 14];
    params[0..2].copy_from_slice(&handle.to_le_bytes());
    params[2..4].copy_from_slice(&TARGET_INTERVAL.to_le_bytes());
    params[4..6].copy_from_slice(&TARGET_INTERVAL.to_le_bytes());
    params[6..8].copy_from_slice(&SLAVE_LATENCY.to_le_bytes());
    params[8..10].copy_from_slice(&SUPERVISION_TIMEOUT.to_le_bytes());
    params[10..12].copy_from_slice(&MIN_CE_LEN.to_le_bytes());
    params[12..14].copy_from_slice(&MAX_CE_LEN.to_le_bytes());
    params
}

pub fn decode_update_complete(evt: &[u8]) -> Option<(u16, u8, u16)> {
    if evt.len() < 7
        || evt[0] != HCI_EVENT_LE_META
        || evt[1] != LE_SUBEVENT_CONNECTION_UPDATE_COMPLETE
    {
        return None;
    }
    let status = evt[2];
    let handle = u16::from_le_bytes([evt[3], evt[4]]);
    let interval = u16::from_le_bytes([evt[5], evt[6]]);
    Some((handle, status, interval))
}

// A raw HCI event is `[event_code, param_len, params...]`, but the pure decoder
// expects the length byte stripped: `[event_code, subevent, status, ...]`.
fn le_update_complete_event(evt: &[u8]) -> Option<(u16, u8, u16)> {
    if evt.len() < 8
        || evt[0] != HCI_EVENT_LE_META
        || evt[2] != LE_SUBEVENT_CONNECTION_UPDATE_COMPLETE
    {
        return None;
    }
    let mut compact = [0u8; 7];
    compact[0] = evt[0];
    compact[1] = evt[2];
    compact[2..].copy_from_slice(&evt[3..8]);
    decode_update_complete(&compact)
}

fn decode_monitor_interval(frame: &[u8]) -> Option<(u16, u16)> {
    if frame.len() < HCI_MON_HDR_SIZE {
        return None;
    }
    let opcode = u16::from_le_bytes([frame[0], frame[1]]);
    if opcode != HCI_MON_EVENT_PKT {
        return None;
    }
    let len = u16::from_le_bytes([frame[4], frame[5]]) as usize;
    if frame.len() < HCI_MON_HDR_SIZE + len {
        return None;
    }
    let evt = &frame[HCI_MON_HDR_SIZE..HCI_MON_HDR_SIZE + len];
    le_update_complete_event(evt).map(|(handle, _, interval)| (handle, interval))
}

fn targeted_interval(our_handle: u16, frame: &[u8]) -> Option<u16> {
    match decode_monitor_interval(frame) {
        Some((handle, interval)) if handle == our_handle => Some(interval),
        _ => None,
    }
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait IntervalControl: Send + Sync {
    async fn enforce(&self) -> Result<()>;
    fn start_guard(&self) -> Result<()>;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrHci {
    family: u16,
    dev: u16,
    channel: u16,
}

// `struct hci_filter` is defined in terms of `unsigned long`, so this layout is
// target-correct for the 32-bit ARM musl deployment but would be malformed on a
// 64-bit host. Unlike the `HCIGETCONNLIST` constant above (split by pointer
// width), the filter is deliberately not dual-width: the HCI path only runs on
// the target, never in host tests.
#[repr(C)]
#[derive(Clone, Copy)]
struct HciFilter {
    type_mask: u32,
    event_mask: [u32; 2],
    opcode: u16,
}

#[repr(C)]
struct HciConnListReq {
    dev_id: u16,
    conn_num: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HciConnInfo {
    handle: u16,
    bdaddr: [u8; 6],
    type_: u8,
    out: u8,
    state: u16,
    link_mode: u32,
}

struct GuardHandle {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

pub struct HciInterval {
    address: String,
    guard: Mutex<Option<GuardHandle>>,
}

impl HciInterval {
    pub fn new(address: String) -> Self {
        Self {
            address,
            guard: Mutex::new(None),
        }
    }
}

impl Drop for HciInterval {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.guard.lock() {
            if let Some(handle) = slot.take() {
                handle.stop.store(true, Ordering::Relaxed);
                let _ = handle.thread.join();
            }
        }
    }
}

#[async_trait]
impl IntervalControl for HciInterval {
    async fn enforce(&self) -> Result<()> {
        let address = self.address.clone();
        // The HCI poll/read is blocking, so keep it off the async runtime.
        tokio::task::spawn_blocking(move || apply_interval(&address))
            .await
            .context("interval enforcement task panicked")?
    }

    fn start_guard(&self) -> Result<()> {
        // Take the lock before opening anything, so a poisoned mutex returns
        // cleanly without leaking a socket or a detached guard thread.
        let mut slot = match self.guard.lock() {
            Ok(slot) => slot,
            Err(_) => return Err(anyhow!("interval guard mutex poisoned")),
        };
        let handle = {
            let raw = open_raw_socket()?;
            let resolved = find_connection_handle(raw, &self.address);
            unsafe { libc::close(raw) };
            resolved?
        };
        let fd = open_monitor_socket()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let address = self.address.clone();
        let thread = match std::thread::Builder::new()
            .name("hci-interval-guard".to_string())
            .spawn(move || guard_loop(fd, handle, address, thread_stop))
        {
            Ok(thread) => thread,
            Err(e) => {
                // The spawn closure (and its fd copy) is dropped on failure;
                // close the caller's fd before returning.
                unsafe { libc::close(fd) };
                return Err(
                    anyhow::Error::from(e).context("failed to spawn HCI interval guard thread")
                );
            }
        };

        if let Some(previous) = slot.take() {
            previous.stop.store(true, Ordering::Relaxed);
            let _ = previous.thread.join();
        }
        *slot = Some(GuardHandle { stop, thread });
        info!("HCI interval guard active");
        Ok(())
    }
}

fn open_raw_socket() -> Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            BTPROTO_HCI,
        )
    };
    if fd < 0 {
        return Err(anyhow!(
            "open HCI raw socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    bind_hci_socket(fd, HCI_DEV, HCI_CHANNEL_RAW)?;

    let filter = HciFilter {
        type_mask: 1 << HCI_EVENT_PKT,
        event_mask: [0xffff_ffff, 0xffff_ffff],
        opcode: 0,
    };
    let ret = unsafe {
        libc::setsockopt(
            fd,
            SOL_HCI,
            HCI_FILTER,
            &filter as *const HciFilter as *const libc::c_void,
            std::mem::size_of::<HciFilter>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow!("set HCI event filter: {}", err));
    }
    Ok(fd)
}

fn open_monitor_socket() -> Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            BTPROTO_HCI,
        )
    };
    if fd < 0 {
        return Err(anyhow!(
            "open HCI monitor socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    bind_hci_socket(fd, HCI_DEV_NONE, HCI_CHANNEL_MONITOR)?;
    Ok(fd)
}

fn bind_hci_socket(fd: RawFd, dev: u16, channel: u16) -> Result<()> {
    let addr = SockaddrHci {
        family: AF_BLUETOOTH as u16,
        dev,
        channel,
    };
    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const SockaddrHci as *const libc::sockaddr,
            std::mem::size_of::<SockaddrHci>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow!(
            "bind HCI socket (dev={dev}, channel={channel}): {err}"
        ));
    }
    Ok(())
}

fn send_hci_command(fd: RawFd, opcode: u16, params: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(4 + params.len());
    frame.push(0x01);
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.push(params.len() as u8);
    frame.extend_from_slice(params);
    let written = unsafe { libc::write(fd, frame.as_ptr() as *const libc::c_void, frame.len()) };
    if written < 0 {
        return Err(anyhow!(
            "write HCI command 0x{opcode:04x}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if written as usize != frame.len() {
        return Err(anyhow!(
            "short write HCI command 0x{opcode:04x}: {written}/{} bytes",
            frame.len()
        ));
    }
    Ok(())
}

fn read_hci_event(fd: RawFd, timeout: Duration) -> Result<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
    if ret < 0 {
        return Err(anyhow!(
            "poll HCI socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    if ret == 0 {
        return Err(anyhow!("timed out waiting for HCI event"));
    }
    let mut buf = [0u8; 1024];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n <= 0 {
        return Err(anyhow!(
            "read HCI event: {}",
            std::io::Error::last_os_error()
        ));
    }
    let data = &buf[..n as usize];
    if data.len() < 2 || data[0] != HCI_EVENT_PKT as u8 {
        return Err(anyhow!(
            "unexpected HCI frame type: 0x{:02x}",
            data.first().copied().unwrap_or(0)
        ));
    }
    Ok(data[1..].to_vec())
}

fn find_connection_handle(fd: RawFd, address: &str) -> Result<u16> {
    const MAX_CONN: usize = 16;
    let req_size = std::mem::size_of::<HciConnListReq>();
    let info_size = std::mem::size_of::<HciConnInfo>();
    let mut buf = vec![0u8; req_size + MAX_CONN * info_size];
    let req = buf.as_mut_ptr() as *mut HciConnListReq;
    unsafe {
        (*req).dev_id = HCI_DEV;
        (*req).conn_num = MAX_CONN as u16;
    }
    let ret = unsafe { libc::ioctl(fd, HCIGETCONNLIST, buf.as_mut_ptr() as *mut libc::c_void) };
    if ret < 0 {
        return Err(anyhow!(
            "HCIGETCONNLIST ioctl: {}",
            std::io::Error::last_os_error()
        ));
    }
    let conn_num = unsafe { (*req).conn_num } as usize;
    let target = address.replace(':', "").to_lowercase();
    let info_ptr = unsafe { buf.as_ptr().add(req_size) as *const HciConnInfo };
    for index in 0..conn_num {
        let conn = unsafe { &*info_ptr.add(index) };
        let b = &conn.bdaddr;
        let found = format!(
            "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[5], b[4], b[3], b[2], b[1], b[0]
        );
        if found == target {
            return Ok(conn.handle);
        }
    }
    Err(anyhow!("device {address} not found in HCI connection list"))
}

fn apply_interval(address: &str) -> Result<()> {
    let fd = open_raw_socket()?;
    let result = (|| {
        let handle = find_connection_handle(fd, address)?;
        send_connection_update(fd, handle)?;
        info!(
            handle = format_args!("{handle:#06x}"),
            "connection interval set to 7.5 ms"
        );
        Ok(())
    })();
    unsafe { libc::close(fd) };
    result
}

fn send_connection_update(fd: RawFd, handle: u16) -> Result<()> {
    let params = conn_update_params(handle);
    send_hci_command(fd, HCI_LE_CONN_UPDATE, &params)?;

    let deadline = Instant::now() + CONN_UPDATE_TIMEOUT;
    let mut command_ok = false;
    let mut complete = false;
    while Instant::now() < deadline {
        let evt = match read_hci_event(fd, HCI_READ_TIMEOUT) {
            Ok(evt) => evt,
            Err(_) => continue,
        };
        if evt.len() < 3 {
            continue;
        }
        if evt[0] == HCI_EVENT_COMMAND_STATUS && evt.len() >= 6 {
            let opcode = u16::from_le_bytes([evt[4], evt[5]]);
            if opcode == HCI_LE_CONN_UPDATE {
                if evt[2] != 0 {
                    return Err(anyhow!(
                        "LE Connection Update command status 0x{:02x}",
                        evt[2]
                    ));
                }
                command_ok = true;
            }
        } else if evt[0] == HCI_EVENT_COMMAND_COMPLETE && evt.len() >= 6 {
            let opcode = u16::from_le_bytes([evt[3], evt[4]]);
            if opcode == HCI_LE_CONN_UPDATE {
                if evt[5] != 0 {
                    return Err(anyhow!(
                        "LE Connection Update command complete status 0x{:02x}",
                        evt[5]
                    ));
                }
                command_ok = true;
            }
        } else if let Some((reported_handle, status, _interval)) = le_update_complete_event(&evt) {
            if reported_handle != handle {
                continue;
            }
            if status != 0 {
                return Err(anyhow!(
                    "LE Connection Update rejected, status 0x{status:02x}"
                ));
            }
            complete = true;
        }
        if command_ok && complete {
            return Ok(());
        }
    }
    if !complete {
        Err(anyhow!(
            "timed out waiting for LE Connection Update Complete"
        ))
    } else {
        Err(anyhow!("timed out waiting for LE Connection Update status"))
    }
}

fn guard_loop(fd: RawFd, handle: u16, address: String, stop: Arc<AtomicBool>) {
    let mut last_attempt: Option<Instant> = None;
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, GUARD_POLL_TIMEOUT_MS) };
        if ret < 0 {
            warn!(
                "HCI monitor poll failed: {}",
                std::io::Error::last_os_error()
            );
            break;
        }
        if ret == 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            warn!(
                "HCI monitor read ended: {}",
                std::io::Error::last_os_error()
            );
            break;
        }
        let Some(interval) = targeted_interval(handle, &buf[..n as usize]) else {
            continue;
        };
        if should_reapply(interval, last_attempt, Instant::now()) == IntervalDecision::Apply {
            last_attempt = Some(Instant::now());
            let address = address.clone();
            std::thread::Builder::new()
                .spawn(move || {
                    if let Err(e) = apply_interval(&address) {
                        warn!("interval re-apply failed: {e:#}");
                    }
                })
                .ok();
        }
    }
    unsafe { libc::close(fd) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reapply_skips_at_target() {
        let now = Instant::now();
        assert_eq!(
            should_reapply(TARGET_INTERVAL, None, now),
            IntervalDecision::Skip
        );
    }

    #[test]
    fn should_reapply_applies_off_target_without_prior_attempt() {
        let now = Instant::now();
        assert_eq!(should_reapply(0x0008, None, now), IntervalDecision::Apply);
    }

    #[test]
    fn should_reapply_suppresses_second_off_target_inside_window() {
        let attempted = Instant::now();
        let now = attempted + Duration::from_secs(1);
        assert_eq!(
            should_reapply(0x0008, Some(attempted), now),
            IntervalDecision::Skip
        );
    }

    #[test]
    fn should_reapply_applies_off_target_after_window() {
        let attempted = Instant::now();
        let now = attempted + REAPPLY_RATE_LIMIT;
        assert_eq!(
            should_reapply(0x0008, Some(attempted), now),
            IntervalDecision::Apply
        );
    }

    #[test]
    fn conn_update_params_serialises_exact_block() {
        assert_eq!(
            conn_update_params(0x0040),
            [0x40, 0x00, 0x06, 0x00, 0x06, 0x00, 0x00, 0x00, 0x64, 0x00, 0x01, 0x00, 0x01, 0x00,]
        );
    }

    #[test]
    fn decode_update_complete_parses_le_meta() {
        let evt = [0x3e, 0x03, 0x00, 0x40, 0x00, 0x06, 0x00];
        assert_eq!(decode_update_complete(&evt), Some((0x0040, 0x00, 0x0006)));
    }

    #[test]
    fn decode_update_complete_rejects_short_or_unrelated() {
        assert_eq!(decode_update_complete(&[]), None);
        assert_eq!(decode_update_complete(&[0x3e, 0x03, 0x00]), None);
        assert_eq!(
            decode_update_complete(&[0x3e, 0x02, 0x00, 0x40, 0x00, 0x06, 0x00]),
            None
        );
        assert_eq!(
            decode_update_complete(&[0x0e, 0x03, 0x00, 0x40, 0x00, 0x06, 0x00]),
            None
        );
    }

    #[test]
    fn decode_monitor_interval_reads_connection_update_complete() {
        let mut frame = vec![0x03, 0x00, 0x00, 0x00, 0x08, 0x00];
        frame.extend_from_slice(&[0x3e, 0x06, 0x03, 0x00, 0x40, 0x00, 0x06, 0x00]);
        assert_eq!(decode_monitor_interval(&frame), Some((0x0040, 0x0006)));
    }

    #[test]
    fn targeted_interval_selects_only_our_handle() {
        let mut frame = vec![0x03, 0x00, 0x00, 0x00, 0x08, 0x00];
        frame.extend_from_slice(&[0x3e, 0x06, 0x03, 0x00, 0x40, 0x00, 0x06, 0x00]);
        assert_eq!(targeted_interval(0x0040, &frame), Some(0x0006));
        assert_eq!(targeted_interval(0x0041, &frame), None);
    }

    #[test]
    fn decode_monitor_interval_ignores_non_event_frames() {
        let frame = [0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0xaa];
        assert_eq!(decode_monitor_interval(&frame), None);
    }
}
