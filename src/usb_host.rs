//! USB Host module — pure Rust, no CDC-ACM component.
//!
//! Drives a PL2303 (or similar vendor-class) USB-serial adapter directly
//! through the ESP-IDF `usb_host_*` API.  Received serial data is forwarded
//! via `log::info!()` (→ syslog when enabled).

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use esp_idf_sys::*;
use log::{info, warn};
use std::thread;
use std::time::Duration;
use crate::gpio_ctrl::GpioPwmState;

// ─── Constants ────────────────────────────────────────────────────────────────

const USB_DESC_TYPE_INTERFACE: u8 = 4;
const USB_DESC_TYPE_ENDPOINT: u8 = 5;
const USB_EP_DIR_IN: u8 = 0x80;
const USB_EP_ATTR_BULK: u8 = 0x02;
const USB_SETUP_PKT_SIZE: usize = 8;
const MAX_USB_PORTS: usize = 4;

// ─── Shared state (accessed from callbacks + client thread) ───────────────────

/// Address of a newly enumerated device; 0 = none pending.
static NEW_DEV_ADDR: AtomicU8 = AtomicU8::new(0);
/// Set when a device we opened is removed.
static DEV_GONE: AtomicBool = AtomicBool::new(false);
/// Set when a control transfer completes (success or failure).
static CTRL_DONE: AtomicBool = AtomicBool::new(false);
/// Status of the last completed control transfer.
static CTRL_STATUS: AtomicU32 = AtomicU32::new(0);
/// Actual bytes transferred by the last control transfer.
static CTRL_ACTUAL: AtomicU32 = AtomicU32::new(0);
/// Counter for bulk-IN rx invocations (diagnostic).
static DATA_RX_COUNT: AtomicU32 = AtomicU32::new(0);
/// Per-port: true while a bulk-OUT transfer is in flight.
static CDC_PORT_OUT_BUSY: [AtomicBool; MAX_USB_PORTS] = [
    AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false),
];
/// Opaque device handle for the currently connected CDC device (0 = none).
static CDC_DEV_HDL: AtomicUsize = AtomicUsize::new(0);
/// VID of currently connected USB device (0 = none).
static CDC_VID: AtomicU32 = AtomicU32::new(0);
/// PID of currently connected USB device (0 = none).
static CDC_PID: AtomicU32 = AtomicU32::new(0);
/// Per-port bulk-OUT endpoint address.
static CDC_PORT_EP_OUT: [AtomicU8; MAX_USB_PORTS] = [
    AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0),
];
/// Per-port pre-allocated bulk-OUT transfer (set in handle_new_device).
static mut CDC_PORT_OUT_XFER: [*mut usb_transfer_t; MAX_USB_PORTS] = [core::ptr::null_mut(); MAX_USB_PORTS];
/// Number of active USB CDC ports (1 for single-port, 2–4 for multi-port FTDI).
static CDC_PORT_COUNT: AtomicU8 = AtomicU8::new(0);
/// Guard: ssh_bridge has already been initialised.
static SSH_BRIDGE_STARTED: AtomicBool = AtomicBool::new(false);
/// Whether USB CDC host is enabled (set at startup from config).
static CDC_ENABLED: AtomicBool = AtomicBool::new(false);
/// CDC baud rate (set at startup from config, default 115200).
static CDC_BAUD: AtomicU32 = AtomicU32::new(115200);
/// True when the connected device is FTDI (needs 2-byte modem status stripping).
static IS_FTDI: AtomicBool = AtomicBool::new(false);
/// Shared GPIO/PWM state — written by SSH callbacks, read by main loop.
static GPIO_PWM_STATE: std::sync::Mutex<Option<GpioPwmState>> = std::sync::Mutex::new(None);
/// Per-port display RX buffers — always fed regardless of which page is shown.
static COM1_DISP_BUF: std::sync::Mutex<Option<crate::serial_display::SerialRxBuffer>> = std::sync::Mutex::new(None);
static COM2_DISP_BUF: std::sync::Mutex<Option<crate::serial_display::SerialRxBuffer>> = std::sync::Mutex::new(None);
static USB_DISP_BUF:  std::sync::Mutex<Option<crate::serial_display::SerialRxBuffer>> = std::sync::Mutex::new(None);
/// Which serial port is shown on the display (kept for httpserver status/WebUI compat).
static DISPLAY_PORT: AtomicU8 = AtomicU8::new(DEVICE_COM1);

// ─── WebSocket sender registry (per-port, for browser serial terminal) ───────
use esp_idf_svc::http::server::ws::EspHttpWsDetachedSender;
use embedded_svc::ws::FrameType;
use std::collections::{HashMap, VecDeque};

/// Per-port list of WS senders.  Each entry is (session_fd, sender).
/// Senders whose `is_closed()` returns true are pruned on each broadcast.
pub static WS_SENDERS_COM1: std::sync::Mutex<Vec<(i32, EspHttpWsDetachedSender)>> = std::sync::Mutex::new(Vec::new());
pub static WS_SENDERS_COM2: std::sync::Mutex<Vec<(i32, EspHttpWsDetachedSender)>> = std::sync::Mutex::new(Vec::new());
pub static WS_SENDERS_USB:  std::sync::Mutex<Vec<(i32, EspHttpWsDetachedSender)>> = std::sync::Mutex::new(Vec::new());

/// Separate fd→device_id map for lock-free port lookup from the httpd handler
/// thread. Using WS_SENDERS_* for this lookup causes a deadlock: the ws_send
/// thread holds WS_SENDERS_* while calling sender.send() (which may block on
/// httpd), while the httpd handler thread needs WS_SENDERS_* to call ws_fd_port.
static WS_FD_PORT_MAP: std::sync::LazyLock<std::sync::Mutex<HashMap<i32, u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Per-port ring buffers for WS data (RX paths write here, sender thread drains).
const WS_RING_CAP: usize = 4096;
static WS_RING_COM1: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());
static WS_RING_COM2: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());
static WS_RING_USB:  std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());

fn ws_senders_for(device_id: u8) -> &'static std::sync::Mutex<Vec<(i32, EspHttpWsDetachedSender)>> {
    match device_id {
        DEVICE_COM1 => &WS_SENDERS_COM1,
        DEVICE_COM2 => &WS_SENDERS_COM2,
        _ => &WS_SENDERS_USB,
    }
}

fn ws_ring_for(device_id: u8) -> &'static std::sync::Mutex<VecDeque<u8>> {
    match device_id {
        DEVICE_COM1 => &WS_RING_COM1,
        DEVICE_COM2 => &WS_RING_COM2,
        _ => &WS_RING_USB,
    }
}

/// Register a WebSocket sender for a serial port.
pub fn ws_register_sender(port_name: &str, fd: i32, sender: EspHttpWsDetachedSender) {
    let device_id = match port_name {
        "com1" => DEVICE_COM1,
        "com2" => DEVICE_COM2,
        _ => DEVICE_USB0,
    };
    let senders = ws_senders_for(device_id);
    let mut list = senders.lock().unwrap();
    list.push((fd, sender));
    if let Ok(mut map) = WS_FD_PORT_MAP.lock() {
        map.insert(fd, device_id);
    }
    info!("[WS] registered sender fd={} for port {}", fd, port_name);
}

/// Remove a WebSocket sender for a serial port.
pub fn ws_remove_sender(port_name: &str, fd: i32) {
    let device_id = match port_name {
        "com1" => DEVICE_COM1,
        "com2" => DEVICE_COM2,
        _ => DEVICE_USB0,
    };
    let senders = ws_senders_for(device_id);
    let mut list = senders.lock().unwrap();
    list.retain(|(f, _)| *f != fd);
    if let Ok(mut map) = WS_FD_PORT_MAP.lock() {
        map.remove(&fd);
    }
    info!("[WS] removed sender fd={} for port {}", fd, port_name);
}

/// Return the port name for a WebSocket fd ("com1", "com2", "usb", or "").
/// Uses the lock-free WS_FD_PORT_MAP so the httpd handler thread can call this
/// without risk of deadlocking against ws_send thread which holds WS_SENDERS_*.
pub fn ws_port_for_fd(fd: i32) -> &'static str {
    if let Ok(map) = WS_FD_PORT_MAP.lock() {
        return match map.get(&fd).copied() {
            Some(d) if d == DEVICE_COM1 => "com1",
            Some(d) if d == DEVICE_COM2 => "com2",
            Some(d) if d != DEVICE_NONE => "usb",
            _ => "",
        };
    }
    ""
}

/// Queue serial RX data for WebSocket broadcast (non-blocking).
/// Called from bulk_in_cb and uart_rx_thread — must never block on httpd.
fn ws_enqueue(device_id: u8, data: &[u8]) {
    if let Ok(mut ring) = ws_ring_for(device_id).try_lock() {
        for &b in data {
            if ring.len() >= WS_RING_CAP {
                // Drop oldest bytes on overflow
                ring.pop_front();
            }
            ring.push_back(b);
        }
    }
    // If lock is contended, silently drop — sender thread holds it briefly.
}

/// Drain one port's ring buffer and broadcast to its WS senders.
/// Called from the dedicated ws_sender_thread only.
fn ws_drain_and_send(device_id: u8) {
    // Drain ring buffer first (short lock)
    let data: Vec<u8> = {
        if let Ok(mut ring) = ws_ring_for(device_id).try_lock() {
            let v: Vec<u8> = ring.drain(..).collect();
            v
        } else {
            return;
        }
    };
    if data.is_empty() {
        return;
    }
    // Send to WS clients (may block on httpd — that's fine here, we're on our own thread)
    let senders = ws_senders_for(device_id);
    let mut list = senders.lock().unwrap();
    list.retain_mut(|(fd, sender): &mut (i32, EspHttpWsDetachedSender)| {
        if sender.is_closed() {
            info!("[WS] pruning closed sender fd={}", fd);
            return false;
        }
        if let Err(e) = sender.send(FrameType::Binary(false), &data) {
            warn!("[WS] send error fd={}: {:?}", fd, e);
            return false;
        }
        true
    });
}

/// Background thread: drains per-port WS ring buffers and sends to clients.
/// Also drains TX queues (browser→serial). Runs ~50 Hz so latency is ≤20 ms.
pub fn start_ws_sender_thread() {
    thread::Builder::new()
        .name("ws_send".into())
        .stack_size(6144)
        .spawn(move || {
            loop {
                ws_drain_and_send(DEVICE_COM1);
                ws_drain_and_send(DEVICE_COM2);
                ws_drain_and_send(DEVICE_USB0);
                ws_drain_tx(DEVICE_COM1);
                ws_drain_tx(DEVICE_COM2);
                ws_drain_tx(DEVICE_USB0);
                thread::sleep(Duration::from_millis(20));
            }
        })
        .expect("spawn ws_sender thread");
}

/// Per-port TX queues for WebSocket→serial direction.
/// The httpd thread enqueues data here (non-blocking); the ws_sender_thread drains it.
static WS_TX_COM1: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());
static WS_TX_COM2: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());
static WS_TX_USB:  std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());

/// Write data to a serial port (for WebSocket TX direction: browser → serial).
/// Non-blocking: enqueues data for the ws_sender_thread to drain.
pub fn serial_write(port_name: &str, data: &[u8]) {
    let queue = match port_name {
        "com1" => &WS_TX_COM1,
        "com2" => &WS_TX_COM2,
        "usb" | "usb0" => &WS_TX_USB,
        _ => return,
    };
    if let Ok(mut q) = queue.try_lock() {
        for &b in data {
            if q.len() >= WS_RING_CAP {
                q.pop_front();
            }
            q.push_back(b);
        }
    }
}

/// Actually write queued TX data to the serial port. Called from ws_sender_thread.
fn ws_drain_tx(device_id: u8) {
    let queue = match device_id {
        DEVICE_COM1 => &WS_TX_COM1,
        DEVICE_COM2 => &WS_TX_COM2,
        _ => &WS_TX_USB,
    };
    let data: Vec<u8> = {
        if let Ok(mut q) = queue.try_lock() {
            q.drain(..).collect()
        } else {
            return;
        }
    };
    if data.is_empty() {
        return;
    }
    match device_id {
        DEVICE_COM1 => {
            if UART1_READY.load(Ordering::Relaxed) {
                unsafe { uart_write_bytes(1, data.as_ptr() as *const c_void, data.len()); }
            }
        }
        DEVICE_COM2 => {
            if UART2_READY.load(Ordering::Relaxed) {
                unsafe { uart_write_bytes(2, data.as_ptr() as *const c_void, data.len()); }
            }
        }
        _ => {
            if CDC_ENABLED.load(Ordering::Relaxed) && CDC_DEV_HDL.load(Ordering::Acquire) != 0 {
                unsafe { usb_cdc_write_port(0, data.as_ptr(), data.len()); }
            }
        }
    }
}

/// Register per-port SerialRxBuffers for the display.
pub fn set_display_rx_bufs(
    com1: crate::serial_display::SerialRxBuffer,
    com2: crate::serial_display::SerialRxBuffer,
    usb: crate::serial_display::SerialRxBuffer,
) {
    *COM1_DISP_BUF.lock().unwrap() = Some(com1);
    *COM2_DISP_BUF.lock().unwrap() = Some(com2);
    *USB_DISP_BUF.lock().unwrap()  = Some(usb);
}

/// Set which port the display should monitor (also switches display page).
pub fn set_display_port(name: &str) {
    let (id, page) = match name {
        "com1" => (DEVICE_COM1, crate::serial_display::PAGE_COM1),
        "com2" => (DEVICE_COM2, crate::serial_display::PAGE_COM2),
        "usb0" => (DEVICE_USB0, crate::serial_display::PAGE_USB),
        "usb1" => (DEVICE_USB1, crate::serial_display::PAGE_USB),
        "usb2" => (DEVICE_USB2, crate::serial_display::PAGE_USB),
        "usb3" => (DEVICE_USB3, crate::serial_display::PAGE_USB),
        _ => return,
    };
    DISPLAY_PORT.store(id, Ordering::SeqCst);
    crate::serial_display::set_page(page);
}

/// Get the name of the port currently shown on the display.
pub fn display_port_name() -> &'static str {
    match DISPLAY_PORT.load(Ordering::Relaxed) {
        DEVICE_USB0 => "usb0",
        DEVICE_USB1 => "usb1",
        DEVICE_USB2 => "usb2",
        DEVICE_USB3 => "usb3",
        DEVICE_COM1 => "com1",
        DEVICE_COM2 => "com2",
        _ => "none",
    }
}

// ─── SSH bridge FFI ───────────────────────────────────────────────────────────

extern "C" {
    fn ssh_bridge_init(port: u16,
                       username: *const core::ffi::c_char,
                       password: *const core::ffi::c_char) -> i32;
    fn ssh_bridge_cdc_rx(data: *const u8, len: usize);
}

// ─── C→Rust logging (so C logs appear in syslog) ─────────────────────────────

#[no_mangle]
pub extern "C" fn ssh_log_info(msg: *const core::ffi::c_char) {
    let s = unsafe { std::ffi::CStr::from_ptr(msg) }.to_str().unwrap_or("?");
    log::info!("[SSH_BRIDGE] {}", s);
}

#[no_mangle]
pub extern "C" fn ssh_log_warn(msg: *const core::ffi::c_char) {
    let s = unsafe { std::ffi::CStr::from_ptr(msg) }.to_str().unwrap_or("?");
    log::warn!("[SSH_BRIDGE] {}", s);
}

/// Log hex dump of data direction + content (called from C bridge loop).
#[no_mangle]
pub extern "C" fn ssh_log_hex(dir: *const core::ffi::c_char, data: *const u8, len: usize) {
    // let d = unsafe { std::ffi::CStr::from_ptr(dir) }.to_str().unwrap_or("?");
    // let slice = unsafe { core::slice::from_raw_parts(data, len.min(32)) };
    // let hex: String = slice.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
    // log::info!("[BRIDGE] {} len={} [{}]", d, len, hex);
    let _ = (dir, data, len);
}

// ─── Device constants and active device tracking ──────────────────────────────

const DEVICE_NONE: u8 = 0;
const DEVICE_USB0: u8 = 1;
const DEVICE_COM1: u8 = 2;
const DEVICE_COM2: u8 = 3;
const DEVICE_USB1: u8 = 4;
const DEVICE_USB2: u8 = 5;
const DEVICE_USB3: u8 = 6;

/// Map USB port index (0–3) to the device-ID constant.
fn usb_port_device_id(port: usize) -> u8 {
    match port {
        0 => DEVICE_USB0,
        1 => DEVICE_USB1,
        2 => DEVICE_USB2,
        3 => DEVICE_USB3,
        _ => DEVICE_NONE,
    }
}

/// Return the number of UART ports an FTDI chip exposes based on PID.
fn ftdi_port_count(pid: u16) -> usize {
    match pid {
        0x6011 => 4, // FT4232H
        0x6010 => 2, // FT2232H/D
        _ => 1,
    }
}

/// Which serial device is currently bridged to the active SSH session.
static ACTIVE_DEVICE: AtomicU8 = AtomicU8::new(DEVICE_NONE);

/// Last device selection error reason (NUL-terminated C string in static storage).
static LAST_SELECT_ERR: std::sync::Mutex<&'static [u8]> = std::sync::Mutex::new(b"Unknown device\0");

/// Track whether each UART port was successfully initialised.
static UART1_READY: AtomicBool = AtomicBool::new(false);
static UART2_READY: AtomicBool = AtomicBool::new(false);

// ─── Device write callbacks (SSH client → serial device TX) ──────────────────

/// SSH client → USB CDC TX.
extern "C" fn usb0_write_cb(data: *const u8, len: usize) {
    // log::info!("[TX] usb0 {} bytes", len);
    unsafe { usb_cdc_write_port(0, data, len); }
}

/// SSH client → USB CDC port 1 TX (multi-port FTDI).
extern "C" fn usb1_write_cb(data: *const u8, len: usize) {
    unsafe { usb_cdc_write_port(1, data, len); }
}

/// SSH client → USB CDC port 2 TX (multi-port FTDI).
extern "C" fn usb2_write_cb(data: *const u8, len: usize) {
    unsafe { usb_cdc_write_port(2, data, len); }
}

/// SSH client → USB CDC port 3 TX (multi-port FTDI).
extern "C" fn usb3_write_cb(data: *const u8, len: usize) {
    unsafe { usb_cdc_write_port(3, data, len); }
}

/// SSH client → UART1 TX.
extern "C" fn com1_write_cb(data: *const u8, len: usize) {
    // log::info!("[TX] com1 {} bytes", len);
    unsafe {
        uart_write_bytes(1, data as *const core::ffi::c_void, len);
    }
}

/// SSH client → UART2 TX.
extern "C" fn com2_write_cb(data: *const u8, len: usize) {
    // log::info!("[TX] com2 {} bytes", len);
    unsafe {
        uart_write_bytes(2, data as *const core::ffi::c_void, len);
    }
}

// ─── Device selection (called from ssh_bridge.c) ─────────────────────────────

/// Helper: try to select a USB CDC port.  Returns the write callback on success.
fn try_select_usb_port(port: usize) -> Option<extern "C" fn(*const u8, usize)> {
    if !CDC_ENABLED.load(Ordering::Relaxed) {
        log::warn!("[SSH] usb{} requested but CDC host is disabled", port);
        *LAST_SELECT_ERR.lock().unwrap() = b"USB CDC host is disabled (cdc_enable=false)\0";
        return None;
    }
    if CDC_DEV_HDL.load(Ordering::Acquire) == 0 {
        log::warn!("[SSH] usb{} requested but no USB device connected", port);
        *LAST_SELECT_ERR.lock().unwrap() = b"No USB device connected\0";
        return None;
    }
    let count = CDC_PORT_COUNT.load(Ordering::Acquire) as usize;
    if port >= count {
        log::warn!("[SSH] usb{} requested but device has only {} port(s)", port, count);
        *LAST_SELECT_ERR.lock().unwrap() = b"Port not available on connected device\0";
        return None;
    }
    let device_id = usb_port_device_id(port);
    ACTIVE_DEVICE.store(device_id, Ordering::SeqCst);
    let cb: extern "C" fn(*const u8, usize) = match port {
        0 => usb0_write_cb,
        1 => usb1_write_cb,
        2 => usb2_write_cb,
        3 => usb3_write_cb,
        _ => return None,
    };
    log::info!("[SSH] device selected: usb{} (USB CDC port {})", port, port);
    Some(cb)
}

/// Activate the requested serial device for the current SSH session.
/// Returns the device's TX write callback, or NULL if the name is unknown.
#[no_mangle]
pub extern "C" fn ssh_select_device(
    name: *const core::ffi::c_char,
) -> Option<extern "C" fn(*const u8, usize)> {
    let name_str = unsafe { std::ffi::CStr::from_ptr(name) }
        .to_str()
        .unwrap_or("");
    match name_str {
        "usb0" | "usb" => try_select_usb_port(0),
        "usb1" => try_select_usb_port(1),
        "usb2" => try_select_usb_port(2),
        "usb3" => try_select_usb_port(3),
        "com1" => {
            if UART1_READY.load(Ordering::Relaxed) {
                ACTIVE_DEVICE.store(DEVICE_COM1, Ordering::SeqCst);
                log::info!("[SSH] device selected: com1 (UART1)");
                Some(com1_write_cb)
            } else {
                log::warn!("[SSH] com1 requested but UART1 not initialised");
                *LAST_SELECT_ERR.lock().unwrap() = b"com1 (UART1) not initialised\0";
                None
            }
        }
        "com2" => {
            if UART2_READY.load(Ordering::Relaxed) {
                ACTIVE_DEVICE.store(DEVICE_COM2, Ordering::SeqCst);
                log::info!("[SSH] device selected: com2 (UART2)");
                Some(com2_write_cb)
            } else {
                log::warn!("[SSH] com2 requested but UART2 not initialised");
                *LAST_SELECT_ERR.lock().unwrap() = b"com2 (UART2) not initialised\0";
                None
            }
        }
        other => {
            log::warn!("[SSH] unknown device: '{}'", other);
            *LAST_SELECT_ERR.lock().unwrap() = b"Unknown device. Available: usb0-usb3, com1, com2\0";
            None
        }
    }
}

/// Deactivate the current serial device when the SSH session ends.
#[no_mangle]
pub extern "C" fn ssh_release_device(_name: *const core::ffi::c_char) {
    ACTIVE_DEVICE.store(DEVICE_NONE, Ordering::SeqCst);
    log::info!("[SSH] device released");
}

/// Set one GPIO output from SSH command.
/// `idx` is 0-based: 0-5 = GPIO4-9 (power 1-6), 6 = GPIO12 (dcpower), `on_off` 1=ON 0=OFF.
/// Returns 0 on success, -1 if no state is registered.
#[no_mangle]
pub extern "C" fn ssh_gpio_set(idx: i32, on_off: i32) -> i32 {
    if idx < 0 || idx > 6 {
        log::warn!("[SSH] ssh_gpio_set: idx {} out of range", idx);
        return -1;
    }
    let guard = GPIO_PWM_STATE.lock().unwrap();
    if let Some(ref state) = *guard {
        state.set_gpio(idx as usize, on_off != 0);
        let pin = crate::gpio_ctrl::GPIO_PIN_MAP[idx as usize];
        log::info!("[SSH] GPIO{} (power {}) -> {}",
            pin, idx + 1, if on_off != 0 { "ON" } else { "OFF" });
        0
    } else {
        log::warn!("[SSH] ssh_gpio_set: GPIO/PWM state not initialised");
        -1
    }
}

/// Set one PWM duty from SSH command: `pwm (1|2) (0-100)`.
/// `ch` is 0-based (0 = GPIO10, 1 = GPIO11), `duty` is 0-100.
/// Returns 0 on success, -1 if no state is registered.
#[no_mangle]
pub extern "C" fn ssh_pwm_set(ch: i32, duty: i32) -> i32 {
    if ch < 0 || ch > 1 {
        log::warn!("[SSH] ssh_pwm_set: ch {} out of range", ch);
        return -1;
    }
    if duty < 0 || duty > 100 {
        log::warn!("[SSH] ssh_pwm_set: duty {} out of range", duty);
        return -1;
    }
    let guard = GPIO_PWM_STATE.lock().unwrap();
    if let Some(ref state) = *guard {
        state.set_pwm(ch as usize, duty as u8);
        log::info!("[SSH] PWM{} (GPIO{}) -> {}%", ch + 1, ch + 10, duty);
        0
    } else {
        log::warn!("[SSH] ssh_pwm_set: GPIO/PWM state not initialised");
        -1
    }
}

/// Return the last device‐selection error as a C string pointer.
#[no_mangle]
pub extern "C" fn ssh_device_error() -> *const u8 {
    let err = LAST_SELECT_ERR.lock().unwrap();
    err.as_ptr()
}

/// Submit a USB bulk-OUT transfer on the given port.
/// Drops silently if a transfer is already in flight on that port.
unsafe fn usb_cdc_write_port(port: usize, data: *const u8, len: usize) {
    if port >= MAX_USB_PORTS { return; }
    let dev_hdl = CDC_DEV_HDL.load(Ordering::Acquire) as usb_device_handle_t;
    if dev_hdl.is_null() { return; }
    if len == 0 { return; }

    // Atomically claim the OUT slot for this port
    if CDC_PORT_OUT_BUSY[port]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return; // Previous transfer still pending — drop this packet
    }

    let xfer = CDC_PORT_OUT_XFER[port];
    if xfer.is_null() {
        CDC_PORT_OUT_BUSY[port].store(false, Ordering::Release);
        return;
    }

    let max = (*xfer).data_buffer_size;
    let copy_len = len.min(max);
    core::ptr::copy_nonoverlapping(data, (*xfer).data_buffer, copy_len);
    (*xfer).num_bytes        = copy_len as i32;
    (*xfer).device_handle    = dev_hdl;
    (*xfer).bEndpointAddress = CDC_PORT_EP_OUT[port].load(Ordering::Relaxed);

    let ret = usb_host_transfer_submit(xfer);
    if ret != ESP_OK as i32 {
        warn!("[USB] bulk-OUT submit failed (port {}): 0x{:x}", port, ret);
        CDC_PORT_OUT_BUSY[port].store(false, Ordering::Release);
    }
}

/// Legacy single-port write (port 0).
#[allow(dead_code)]
unsafe fn usb_cdc_write(data: *const u8, len: usize) {
    usb_cdc_write_port(0, data, len);
}

// ─── Callbacks ────────────────────────────────────────────────────────────────

/// Client event callback — fires from `usb_host_client_handle_events`.
unsafe extern "C" fn client_event_cb(
    event_msg: *const usb_host_client_event_msg_t,
    _arg: *mut c_void,
) {
    let msg = &*event_msg;
    #[allow(non_upper_case_globals)]
    match msg.event {
        usb_host_client_event_t_USB_HOST_CLIENT_EVENT_NEW_DEV => {
            let addr = msg.__bindgen_anon_1.new_dev.address;
            info!("[USB] NEW_DEV addr={}", addr);
            NEW_DEV_ADDR.store(addr, Ordering::SeqCst);
        }
        usb_host_client_event_t_USB_HOST_CLIENT_EVENT_DEV_GONE => {
            warn!("[USB] DEV_GONE");
            DEV_GONE.store(true, Ordering::SeqCst);
        }
        _ => {}
    }
}

/// Control-transfer completion callback.
unsafe extern "C" fn ctrl_xfer_cb(transfer: *mut usb_transfer_t) {
    let xfer = &*transfer;
    CTRL_STATUS.store(xfer.status, Ordering::SeqCst);
    CTRL_ACTUAL.store(xfer.actual_num_bytes as u32, Ordering::SeqCst);
    CTRL_DONE.store(true, Ordering::SeqCst);
}

/// Bulk-OUT transfer completion callback (port-aware via context).
unsafe extern "C" fn bulk_out_cb(transfer: *mut usb_transfer_t) {
    let xfer = &*transfer;
    let port = xfer.context as usize;
    if xfer.status != usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED {
        warn!("[USB] bulk OUT error status={} port={}", xfer.status, port);
    }
    // Release the TX slot so the next write can proceed
    if port < MAX_USB_PORTS {
        CDC_PORT_OUT_BUSY[port].store(false, Ordering::Release);
    }
}

/// Bulk-IN transfer completion callback (port-aware via context).
unsafe extern "C" fn bulk_in_cb(transfer: *mut usb_transfer_t) {
    let xfer = &*transfer;
    let port = xfer.context as usize;
    let device_id = usb_port_device_id(port);
    if xfer.status == usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED
        && xfer.actual_num_bytes > 0
    {
        let len = xfer.actual_num_bytes as usize;
        let count = DATA_RX_COUNT.fetch_add(1, Ordering::Relaxed);
        let data = core::slice::from_raw_parts(xfer.data_buffer, len);
        if count == 0 {
            info!("[USB] bulk IN active (first RX, {} bytes, port {})", len, port);
        }
        // FTDI prepends 2 modem-status bytes to every bulk-IN packet; strip them.
        let (fwd_ptr, fwd_len) = if IS_FTDI.load(Ordering::Relaxed) {
            if len <= 2 {
                // Only modem status, no payload — skip forwarding
                (core::ptr::null(), 0)
            } else {
                (data.as_ptr().add(2), len - 2)
            }
        } else {
            (data.as_ptr(), len)
        };
        // Forward received data to the SSH bridge (USB CDC → SSH)
        if fwd_len > 0 && ACTIVE_DEVICE.load(Ordering::Relaxed) == device_id {
            // info!("[RX] usb{} {} bytes", port, fwd_len);
            ssh_bridge_cdc_rx(fwd_ptr, fwd_len);
        }
        // Mirror to USB display buffer (always, regardless of display page)
        if fwd_len > 0 {
            if let Some(ref buf) = *USB_DISP_BUF.lock().unwrap() {
                buf.push_data(core::slice::from_raw_parts(fwd_ptr, fwd_len));
            }
            // Broadcast to WebSocket terminals monitoring USB port
            ws_enqueue(device_id, core::slice::from_raw_parts(fwd_ptr, fwd_len));
        }
    } else if xfer.status != usb_transfer_status_t_USB_TRANSFER_STATUS_CANCELED {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            warn!("[USB] bulk IN error status={} port={}", xfer.status, port);
        }
    }
    // Re-submit for next packet (unless device is gone)
    if !DEV_GONE.load(Ordering::Relaxed) {
        usb_host_transfer_submit(transfer);
    }
}

// ─── Control transfer helper ──────────────────────────────────────────────────

/// Submit a control transfer and pump `usb_host_client_handle_events` until the
/// completion callback fires.  Returns `(esp_err_t, actual_payload_bytes)`.
///
/// This replaces the CDC-ACM component's `send_custom_request` and avoids its
/// stale-semaphore bug entirely.
unsafe fn ctrl_transfer_sync(
    client: usb_host_client_handle_t,
    xfer: *mut usb_transfer_t,
    dev_hdl: usb_device_handle_t,
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
    data_out: &[u8],
    data_in: &mut [u8],
    timeout_ms: u32,
) -> (i32, usize) {
    let xr = &mut *xfer;

    // Build setup packet at the start of data_buffer
    let buf = xr.data_buffer;
    *buf.add(0) = bm_request_type;
    *buf.add(1) = b_request;
    *buf.add(2) = (w_value & 0xFF) as u8;
    *buf.add(3) = (w_value >> 8) as u8;
    *buf.add(4) = (w_index & 0xFF) as u8;
    *buf.add(5) = (w_index >> 8) as u8;
    *buf.add(6) = (w_length & 0xFF) as u8;
    *buf.add(7) = (w_length >> 8) as u8;

    // For OUT: copy payload after setup packet
    let is_in = (bm_request_type & 0x80) != 0;
    if !is_in && !data_out.is_empty() {
        core::ptr::copy_nonoverlapping(
            data_out.as_ptr(),
            buf.add(USB_SETUP_PKT_SIZE),
            data_out.len().min(w_length as usize),
        );
    }

    xr.num_bytes = (USB_SETUP_PKT_SIZE + w_length as usize) as i32;
    xr.device_handle = dev_hdl;
    xr.bEndpointAddress = 0;
    xr.callback = Some(ctrl_xfer_cb);
    xr.timeout_ms = 0;

    CTRL_DONE.store(false, Ordering::SeqCst);

    let ret = usb_host_transfer_submit_control(client, xfer);
    if ret != ESP_OK as i32 {
        return (ret, 0);
    }

    // Pump client events until the callback fires or we time out
    let start = std::time::Instant::now();
    while !CTRL_DONE.load(Ordering::SeqCst) {
        if start.elapsed().as_millis() as u32 > timeout_ms {
            warn!(
                "[USB] ctrl timeout bmReq=0x{:02x} bReq=0x{:02x}",
                bm_request_type, b_request
            );
            return (0x107, 0); // ESP_ERR_TIMEOUT
        }
        usb_host_client_handle_events(client, 50);
    }

    let status = CTRL_STATUS.load(Ordering::SeqCst);
    if status != usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED {
        info!("[USB] ctrl xfer status={} (bmReq=0x{:02x} bReq=0x{:02x})",
              status, bm_request_type, b_request);
        return (ESP_FAIL as i32, 0);
    }

    let actual = CTRL_ACTUAL.load(Ordering::SeqCst) as usize;

    // For IN: copy received payload (after the 8-byte setup packet)
    if is_in && actual > USB_SETUP_PKT_SIZE {
        let payload_len = actual - USB_SETUP_PKT_SIZE;
        let copy_len = payload_len.min(data_in.len());
        core::ptr::copy_nonoverlapping(
            (*xfer).data_buffer.add(USB_SETUP_PKT_SIZE),
            data_in.as_mut_ptr(),
            copy_len,
        );
        return (ESP_OK as i32, copy_len);
    }

    (ESP_OK as i32, 0)
}

// ─── PL2303 init ──────────────────────────────────────────────────────────────

/// Vendor-specific read (bmRequestType = 0xC0, bRequest = 0x01).
unsafe fn pl2303_vendor_read(
    client: usb_host_client_handle_t,
    xfer: *mut usb_transfer_t,
    dev: usb_device_handle_t,
    value: u16,
) -> u8 {
    let mut buf = [0u8; 1];
    let (ret, n) = ctrl_transfer_sync(client, xfer, dev, 0xC0, 0x01, value, 0, 1, &[], &mut buf, 2000);
    if ret != ESP_OK as i32 || n == 0 {
        warn!("[PL] vendor_read(0x{:04x}) failed: 0x{:x} n={}", value, ret, n);
    }
    buf[0]
}

/// Vendor-specific write (bmRequestType = 0x40, bRequest = 0x01).
unsafe fn pl2303_vendor_write(
    client: usb_host_client_handle_t,
    xfer: *mut usb_transfer_t,
    dev: usb_device_handle_t,
    value: u16,
    index: u16,
) {
    let (ret, _) = ctrl_transfer_sync(client, xfer, dev, 0x40, 0x01, value, index, 0, &[], &mut [], 2000);
    if ret != ESP_OK as i32 {
        warn!("[PL] vendor_write(0x{:04x}, 0x{:04x}) failed: 0x{:x}", value, index, ret);
    }
}

unsafe fn pl2303_init(
    client: usb_host_client_handle_t,
    ctrl_xfer: *mut usb_transfer_t,
    dev_hdl: usb_device_handle_t,
    baud_rate: u32,
) {
    // ── Vendor-specific startup sequence (from Linux pl2303 driver) ──
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8484);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0404, 0);
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8484);
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8383);
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8484);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0404, 1);
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8484);
    pl2303_vendor_read(client, ctrl_xfer, dev_hdl, 0x8383);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0000, 1);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0001, 0);
    // HX type: 0x44 enables UART TX/RX
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0002, 0x44);
    info!("[USB] PL2303 vendor init done");

    // ── SetLineCoding 8N1 (class OUT, bRequest=0x20) ──
    let mut slc = [0u8; 7];
    slc[0..4].copy_from_slice(&baud_rate.to_le_bytes());
    slc[4] = 0; // 1 stop bit
    slc[5] = 0; // no parity
    slc[6] = 8; // 8 data bits
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        0x21, 0x20, 0, 0, 7,
        &slc, &mut [], 2000,
    );
    info!("[USB] PL2303 SetLineCoding {} 8N1: 0x{:x}", baud_rate, ret);

    // ── Post-SetLineCoding vendor writes (HX) ──
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0000, 0x01);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0001, 0x00);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0002, 0x44);

    // ── Flush RX/TX buffers: vendor_write(8,0) and vendor_write(9,0) ──
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0008, 0);
    pl2303_vendor_write(client, ctrl_xfer, dev_hdl, 0x0009, 0);

    // ── SetControlLineState DTR+RTS (class OUT, bRequest=0x22) ──
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        0x21, 0x22, 0x0003, 0, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] PL2303 SetControlLineState DTR+RTS: 0x{:x}", ret);

    // ── Verify: GetLineCoding ──
    let mut lc_buf = [0u8; 7];
    let (ret, n) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        0xA1, 0x21, 0, 0, 7,
        &[], &mut lc_buf, 2000,
    );
    if ret == ESP_OK as i32 && n >= 7 {
        let baud = u32::from_le_bytes([lc_buf[0], lc_buf[1], lc_buf[2], lc_buf[3]]);
        info!(
            "[USB] PL2303 GetLineCoding: baud={} stop={} par={} bits={}",
            baud, lc_buf[4], lc_buf[5], lc_buf[6]
        );
    }

    info!("[USB] PL2303 init complete");
}

// ─── CP210x init ──────────────────────────────────────────────────────────────

/// CP210x vendor request codes
const CP210X_REQTYPE_H2D: u8 = 0x41; // host-to-device, vendor, interface
const CP210X_IFC_ENABLE: u8  = 0x00;
const CP210X_SET_LINE_CTL: u8 = 0x03;
const CP210X_SET_MHS: u8     = 0x07;
const CP210X_PURGE: u8       = 0x12;
const CP210X_SET_BAUDRATE: u8 = 0x1E;

unsafe fn cp210x_init(
    client: usb_host_client_handle_t,
    ctrl_xfer: *mut usb_transfer_t,
    dev_hdl: usb_device_handle_t,
    intf: u8,
    baud_rate: u32,
) {
    let idx = intf as u16;

    // 1. Enable UART
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        CP210X_REQTYPE_H2D, CP210X_IFC_ENABLE,
        0x0001, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] CP210x IFC_ENABLE: 0x{:x}", ret);

    // 2. Set baud rate
    let baud = baud_rate.to_le_bytes();
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        CP210X_REQTYPE_H2D, CP210X_SET_BAUDRATE,
        0, idx, 4,
        &baud, &mut [], 2000,
    );
    info!("[USB] CP210x SET_BAUDRATE {}: 0x{:x}", baud_rate, ret);

    // 3. Set line control 8N1  (wValue = data_bits<<8 | parity<<4 | stop)
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        CP210X_REQTYPE_H2D, CP210X_SET_LINE_CTL,
        0x0800, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] CP210x SET_LINE_CTL 8N1: 0x{:x}", ret);

    // 4. Assert DTR + RTS  (bits 0-1 = state, bits 8-9 = write-enable mask)
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        CP210X_REQTYPE_H2D, CP210X_SET_MHS,
        0x0303, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] CP210x SET_MHS DTR+RTS: 0x{:x}", ret);

    // 5. Purge both TX and RX FIFOs
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        CP210X_REQTYPE_H2D, CP210X_PURGE,
        0x000F, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] CP210x PURGE: 0x{:x}", ret);

    info!("[USB] CP210x init complete");
}

// ─── FTDI init ────────────────────────────────────────────────────────────────

/// FTDI vendor request codes
const FTDI_REQTYPE_H2D: u8     = 0x40; // host-to-device, vendor, device
const FTDI_SIO_RESET: u8       = 0x00;
const FTDI_SIO_MODEM_CTRL: u8  = 0x01;
const FTDI_SIO_SET_FLOW_CTRL: u8 = 0x02;
const FTDI_SIO_SET_BAUD_RATE: u8 = 0x03;
const FTDI_SIO_SET_DATA: u8    = 0x04;
const FTDI_SIO_SET_LATENCY: u8 = 0x09;

/// Convert a baud rate to FTDI divisor value (3 MHz base clock).
fn ftdi_baud_to_divisor(baud: u32) -> u16 {
    const BASE: u32 = 3_000_000;
    if baud >= BASE { return 0; }
    if baud >= 2_000_000 { return 1; }
    ((BASE + baud / 2) / baud) as u16
}

unsafe fn ftdi_init(
    client: usb_host_client_handle_t,
    ctrl_xfer: *mut usb_transfer_t,
    dev_hdl: usb_device_handle_t,
    intf: u8,
    baud_rate: u32,
) {
    // FTDI interface index is 1-based in wIndex
    let idx = (intf as u16) + 1;

    // 1. Reset SIO
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_RESET,
        0, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI SIO_RESET: 0x{:x}", ret);

    // 2. Set baud rate (3 MHz base clock)
    let ftdi_divisor = ftdi_baud_to_divisor(baud_rate);
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_SET_BAUD_RATE,
        ftdi_divisor, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI SET_BAUD_RATE {}: 0x{:x}", baud_rate, ret);

    // 3. Set line properties: 8 data bits, no parity, 1 stop bit
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_SET_DATA,
        0x0008, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI SET_DATA 8N1: 0x{:x}", ret);

    // 4. Disable flow control
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_SET_FLOW_CTRL,
        0, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI SET_FLOW_CTRL none: 0x{:x}", ret);

    // 5. Set modem control: assert DTR + RTS
    //    wValue bit 0=DTR value, bit 8=DTR enable, bit 1=RTS value, bit 9=RTS enable
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_MODEM_CTRL,
        0x0303, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI MODEM_CTRL DTR+RTS: 0x{:x}", ret);

    // 6. Set latency timer to 16 ms
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_SET_LATENCY,
        16, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI SET_LATENCY 16ms: 0x{:x}", ret);

    // 7. Purge RX and TX buffers
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_RESET,
        1, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI PURGE_RX: 0x{:x}", ret);
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        FTDI_REQTYPE_H2D, FTDI_SIO_RESET,
        2, idx, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] FTDI PURGE_TX: 0x{:x}", ret);

    info!("[USB] FTDI init complete");
}

// ─── Generic CDC-ACM init (fallback) ─────────────────────────────────────────

/// Standard CDC-ACM SetLineCoding + SetControlLineState for devices that
/// conform to the USB CDC Abstract Control Model specification.
///
/// `comm_intf` is the CDC Communication Class Interface number to use as
/// wIndex in class requests (0 for single-interface devices, typically
/// data_intf - 1 for 2-interface devices like 0x0525/0xA4A7).
unsafe fn cdc_acm_init(
    client: usb_host_client_handle_t,
    ctrl_xfer: *mut usb_transfer_t,
    dev_hdl: usb_device_handle_t,
    comm_intf: u8,
    baud_rate: u32,
) {
    let wi = comm_intf as u16;

    // SetLineCoding: 8N1
    let mut slc = [0u8; 7];
    slc[0..4].copy_from_slice(&baud_rate.to_le_bytes());
    slc[4] = 0; // 1 stop bit
    slc[5] = 0; // no parity
    slc[6] = 8; // 8 data bits
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        0x21, 0x20, 0, wi, 7,
        &slc, &mut [], 2000,
    );
    info!("[USB] CDC-ACM SetLineCoding {} 8N1 (intf {}): 0x{:x}", baud_rate, comm_intf, ret);

    // SetControlLineState: DTR + RTS
    let (ret, _) = ctrl_transfer_sync(
        client, ctrl_xfer, dev_hdl,
        0x21, 0x22, 0x0003, wi, 0,
        &[], &mut [], 2000,
    );
    info!("[USB] CDC-ACM SetControlLineState DTR+RTS: 0x{:x}", ret);

    info!("[USB] CDC-ACM init complete");
}

// ─── Descriptor parsing ───────────────────────────────────────────────────────

/// Bulk endpoint info.
struct BulkEndpoints {
    intf_num: u8,
    ep_in: u8,
    ep_out: u8,
    mps: u16,
}

/// Walk the config descriptor blob and find ALL bulk-IN/OUT endpoint pairs,
/// one per interface.  Returns a `Vec<BulkEndpoints>` — one entry for every
/// interface that has both a bulk-IN and a bulk-OUT endpoint.
unsafe fn find_all_bulk_eps(config_desc: *const usb_config_desc_t) -> Vec<BulkEndpoints> {
    let total_len = (*config_desc).__bindgen_anon_1.wTotalLength as usize;
    let base = config_desc as *const u8;
    let mut offset = 0usize;
    let mut results: Vec<BulkEndpoints> = Vec::new();
    let mut cur_intf: u8 = 0;
    let mut cur_ep_in: Option<(u8, u16)> = None;   // (addr, mps)
    let mut cur_ep_out: Option<u8> = None;          // addr
    let mut seen_first_intf = false;

    while offset + 2 <= total_len {
        let b_length = *base.add(offset) as usize;
        let b_desc_type = *base.add(offset + 1);
        if b_length == 0 { break; }

        if b_desc_type == USB_DESC_TYPE_INTERFACE && offset + 9 <= total_len {
            // Flush previous interface if it had both endpoints
            if seen_first_intf {
                if let (Some((in_addr, mps)), Some(out_addr)) = (cur_ep_in, cur_ep_out) {
                    results.push(BulkEndpoints {
                        intf_num: cur_intf,
                        ep_in: in_addr,
                        ep_out: out_addr,
                        mps,
                    });
                }
            }
            cur_intf = *base.add(offset + 2);
            cur_ep_in = None;
            cur_ep_out = None;
            seen_first_intf = true;
        }
        if b_desc_type == USB_DESC_TYPE_ENDPOINT && offset + 7 <= total_len {
            let ep_addr = *base.add(offset + 2);
            let bm_attr = *base.add(offset + 3);
            let mps = u16::from_le_bytes([*base.add(offset + 4), *base.add(offset + 5)]);
            if (bm_attr & 0x03) == USB_EP_ATTR_BULK {
                if (ep_addr & USB_EP_DIR_IN) != 0 && cur_ep_in.is_none() {
                    cur_ep_in = Some((ep_addr, mps));
                } else if (ep_addr & USB_EP_DIR_IN) == 0 && cur_ep_out.is_none() {
                    cur_ep_out = Some(ep_addr);
                }
            }
        }
        offset += b_length;
    }
    // Flush last interface
    if let (Some((in_addr, mps)), Some(out_addr)) = (cur_ep_in, cur_ep_out) {
        results.push(BulkEndpoints {
            intf_num: cur_intf,
            ep_in: in_addr,
            ep_out: out_addr,
            mps,
        });
    }
    results
}

// ─── USB host daemon task ─────────────────────────────────────────────────────

fn usb_host_daemon() {
    let config = usb_host_config_t {
        skip_phy_setup: false,
        intr_flags: 0,
        ..Default::default()
    };
    let ret = unsafe { usb_host_install(&config) };
    if ret != ESP_OK as i32 {
        warn!("[USB] usb_host_install failed: 0x{:x}", ret);
        return;
    }
    info!("[USB] USB host library installed");
    loop {
        let mut flags: u32 = 0;
        unsafe {
            usb_host_lib_handle_events(u32::MAX, &mut flags);
        }
    }
}

// ─── Client task ──────────────────────────────────────────────────────────────

fn usb_client_task() {
    thread::sleep(Duration::from_millis(500));

    // Register a client
    let mut client: usb_host_client_handle_t = core::ptr::null_mut();
    unsafe {
        let mut async_cfg: usb_host_client_config_t__bindgen_ty_1__bindgen_ty_1 =
            core::mem::zeroed();
        async_cfg.client_event_callback = Some(client_event_cb);
        async_cfg.callback_arg = core::ptr::null_mut();

        let cfg = usb_host_client_config_t {
            is_synchronous: false,
            max_num_event_msg: 5,
            __bindgen_anon_1: usb_host_client_config_t__bindgen_ty_1 { async_: async_cfg },
        };
        let ret = usb_host_client_register(&cfg, &mut client);
        if ret != ESP_OK as i32 {
            warn!("[USB] client_register failed: 0x{:x}", ret);
            return;
        }
    }
    info!("[USB] Client registered, waiting for device...");

    // Allocate a reusable control transfer (setup packet + up to 64 bytes data)
    let mut ctrl_xfer: *mut usb_transfer_t = core::ptr::null_mut();
    unsafe {
        let ret = usb_host_transfer_alloc(64 + USB_SETUP_PKT_SIZE, 0, &mut ctrl_xfer);
        if ret != ESP_OK as i32 || ctrl_xfer.is_null() {
            warn!("[USB] ctrl transfer alloc failed: 0x{:x}", ret);
            return;
        }
    }

    // Check for devices already connected before client was registered
    // (NEW_DEV event only fires for devices enumerated *after* registration)
    unsafe {
        let mut addrs = [0u8; 8];
        let mut num: i32 = 0;
        if usb_host_device_addr_list_fill(8, addrs.as_mut_ptr(), &mut num) == ESP_OK as i32
            && num > 0
        {
            info!("[USB] {} device(s) already connected", num);
            NEW_DEV_ADDR.store(addrs[0], Ordering::SeqCst);
        }
    }

    loop {
        // Pump client events (fires client_event_cb for NEW_DEV / DEV_GONE)
        unsafe {
            usb_host_client_handle_events(client, 200);
        }

        // Handle NEW_DEV (or pre-existing device)
        let addr = NEW_DEV_ADDR.swap(0, Ordering::SeqCst);
        if addr == 0 {
            continue;
        }

        info!("[USB] Opening device addr={}", addr);
        unsafe {
            handle_new_device(client, ctrl_xfer, addr);
        }
        // After handle_new_device returns (device removed), loop back
        DATA_RX_COUNT.store(0, Ordering::Relaxed);
    }
}

/// Open device, parse descriptors, claim interface(s), init chip, start bulk-IN.
/// Supports multi-port FTDI devices (FT4232H = 4 ports, FT2232 = 2 ports).
unsafe fn handle_new_device(
    client: usb_host_client_handle_t,
    ctrl_xfer: *mut usb_transfer_t,
    addr: u8,
) {
    // Open
    let mut dev_hdl: usb_device_handle_t = core::ptr::null_mut();
    let ret = usb_host_device_open(client, addr, &mut dev_hdl);
    if ret != ESP_OK as i32 {
        warn!("[USB] device_open failed: 0x{:x}", ret);
        return;
    }

    // Device descriptor
    let mut desc_ptr: *const usb_device_desc_t = core::ptr::null();
    if usb_host_get_device_descriptor(dev_hdl, &mut desc_ptr) == ESP_OK as i32
        && !desc_ptr.is_null()
    {
        let d = &(*desc_ptr).__bindgen_anon_1;
        let vid = { d.idVendor };
        let pid = { d.idProduct };
        let bcd = { d.bcdDevice };
        let cls = { d.bDeviceClass };
        CDC_VID.store(vid as u32, Ordering::Release);
        CDC_PID.store(pid as u32, Ordering::Release);
        info!(
            "[USB] VID=0x{:04x} PID=0x{:04x} bcdDevice=0x{:04x} class=0x{:02x}",
            vid, pid, bcd, cls
        );
    }

    // Config descriptor
    let mut config_desc: *const usb_config_desc_t = core::ptr::null();
    let ret = usb_host_get_active_config_descriptor(dev_hdl, &mut config_desc);
    if ret != ESP_OK as i32 || config_desc.is_null() {
        warn!("[USB] get config desc failed: 0x{:x}", ret);
        usb_host_device_close(client, dev_hdl);
        return;
    }

    // Find ALL bulk endpoint pairs (one per interface)
    let all_eps = find_all_bulk_eps(config_desc);
    if all_eps.is_empty() {
        warn!("[USB] No bulk endpoints found");
        usb_host_device_close(client, dev_hdl);
        return;
    }

    // Determine number of ports to use
    let vid = CDC_VID.load(Ordering::Acquire) as u16;
    let pid = CDC_PID.load(Ordering::Acquire) as u16;
    let baud = CDC_BAUD.load(Ordering::Relaxed);
    IS_FTDI.store(vid == 0x0403, Ordering::Release);

    let port_count = if vid == 0x0403 {
        ftdi_port_count(pid).min(all_eps.len()).min(MAX_USB_PORTS)
    } else {
        1
    };
    let eps_to_use = &all_eps[..port_count];

    info!("[USB] {} port(s) detected (VID=0x{:04x} PID=0x{:04x})", port_count, vid, pid);

    // Claim interfaces, perform chip-specific init, allocate transfers
    let mut in_xfers: [*mut usb_transfer_t; MAX_USB_PORTS] = [core::ptr::null_mut(); MAX_USB_PORTS];
    let mut out_xfers: [*mut usb_transfer_t; MAX_USB_PORTS] = [core::ptr::null_mut(); MAX_USB_PORTS];
    let mut claimed_intfs: [i8; MAX_USB_PORTS] = [-1; MAX_USB_PORTS]; // -1 = not claimed
    // For CDC-ACM devices with separate comm+data interfaces: the communication
    // class interface (intf_num-1) must also be claimed so that class requests
    // (SetLineCoding, SetControlLineState) are accepted by the USB host stack.
    let mut comm_intfs: [i8; MAX_USB_PORTS] = [-1; MAX_USB_PORTS]; // -1 = not claimed
    let mut active_ports: usize = 0;

    for (port_idx, eps) in eps_to_use.iter().enumerate() {
        // Claim interface
        let ret = usb_host_interface_claim(client, dev_hdl, eps.intf_num, 0);
        if ret != ESP_OK as i32 {
            warn!("[USB] interface_claim({}) failed: 0x{:x}", eps.intf_num, ret);
            continue;
        }
        claimed_intfs[port_idx] = eps.intf_num as i8;

        info!(
            "[USB] Port {} — Bulk-IN 0x{:02x} Bulk-OUT 0x{:02x} MPS={} intf={}",
            port_idx, eps.ep_in, eps.ep_out, eps.mps, eps.intf_num
        );

        // For standard CDC-ACM devices (not vendor-specific FTDI/PL2303/CP210x):
        // if the data interface is intf > 0, the preceding interface is the CDC
        // Communication Class Interface (CCI).  Claim it so the USB host stack
        // will accept class requests (SetLineCoding / SetControlLineState) directed
        // to it, and so the device sees a fully-configured CDC port.
        let comm_intf: u8 = if eps.intf_num > 0 && !matches!(vid, 0x0403 | 0x067B | 0x10C4) {
            let ci = eps.intf_num - 1;
            let r = usb_host_interface_claim(client, dev_hdl, ci, 0);
            if r == ESP_OK as i32 {
                info!("[USB] Claimed CDC comm intf {}", ci);
                comm_intfs[port_idx] = ci as i8;
                ci
            } else {
                info!("[USB] CDC comm intf {} not claimed: 0x{:x} (non-fatal)", ci, r);
                0
            }
        } else {
            0
        };

        // Chip-specific init
        match vid {
            0x067B => pl2303_init(client, ctrl_xfer, dev_hdl, baud),
            0x10C4 => cp210x_init(client, ctrl_xfer, dev_hdl, eps.intf_num, baud),
            0x0403 => ftdi_init(client, ctrl_xfer, dev_hdl, eps.intf_num, baud),
            _      => cdc_acm_init(client, ctrl_xfer, dev_hdl, comm_intf, baud),
        }

        // Allocate bulk-IN transfer
        let buf_size = eps.mps as usize;
        let mut in_xfer: *mut usb_transfer_t = core::ptr::null_mut();
        let ret = usb_host_transfer_alloc(buf_size, 0, &mut in_xfer);
        if ret != ESP_OK as i32 || in_xfer.is_null() {
            warn!("[USB] bulk-IN alloc failed for port {}: 0x{:x}", port_idx, ret);
            continue;
        }
        (*in_xfer).device_handle = dev_hdl;
        (*in_xfer).bEndpointAddress = eps.ep_in;
        (*in_xfer).callback = Some(bulk_in_cb);
        (*in_xfer).num_bytes = buf_size as i32;
        (*in_xfer).timeout_ms = 0;
        (*in_xfer).context = port_idx as *mut c_void;
        in_xfers[port_idx] = in_xfer;

        // Allocate bulk-OUT transfer
        let mut out_xfer: *mut usb_transfer_t = core::ptr::null_mut();
        let ret = usb_host_transfer_alloc(buf_size, 0, &mut out_xfer);
        if ret == ESP_OK as i32 && !out_xfer.is_null() {
            (*out_xfer).callback = Some(bulk_out_cb);
            (*out_xfer).timeout_ms = 0;
            (*out_xfer).context = port_idx as *mut c_void;
            out_xfers[port_idx] = out_xfer;
            CDC_PORT_OUT_XFER[port_idx] = out_xfer;
        } else {
            warn!("[USB] bulk-OUT alloc failed for port {}: 0x{:x} — TX disabled", port_idx, ret);
        }

        CDC_PORT_EP_OUT[port_idx].store(eps.ep_out, Ordering::Release);
        active_ports += 1;
    }

    if active_ports == 0 {
        warn!("[USB] No ports could be initialised");
        usb_host_device_close(client, dev_hdl);
        return;
    }

    // Publish state so write callbacks / SSH device selection can find the device
    CDC_PORT_COUNT.store(active_ports as u8, Ordering::Release);
    CDC_DEV_HDL.store(dev_hdl as usize, Ordering::Release);

    // Submit all bulk-IN transfers
    DEV_GONE.store(false, Ordering::SeqCst);
    for port_idx in 0..active_ports {
        let in_xfer = in_xfers[port_idx];
        if !in_xfer.is_null() {
            let ret = usb_host_transfer_submit(in_xfer);
            if ret != ESP_OK as i32 {
                warn!("[USB] bulk-IN submit failed for port {}: 0x{:x}", port_idx, ret);
            }
        }
    }

    info!("[USB] {} port(s) active, bulk-IN polling started", active_ports);

    // Pump events until device is removed
    loop {
        usb_host_client_handle_events(client, 200);
        if DEV_GONE.load(Ordering::SeqCst) {
            info!("[USB] Device gone — cleaning up {} port(s)", active_ports);
            // Invalidate CDC TX state before freeing
            CDC_DEV_HDL.store(0, Ordering::Release);
            CDC_VID.store(0, Ordering::Release);
            CDC_PID.store(0, Ordering::Release);
            IS_FTDI.store(false, Ordering::Release);
            CDC_PORT_COUNT.store(0, Ordering::Release);

            for i in 0..MAX_USB_PORTS {
                CDC_PORT_OUT_BUSY[i].store(false, Ordering::Release);
                CDC_PORT_EP_OUT[i].store(0, Ordering::Release);
                CDC_PORT_OUT_XFER[i] = core::ptr::null_mut();
            }
            for i in 0..active_ports {
                if !out_xfers[i].is_null() {
                    usb_host_transfer_free(out_xfers[i]);
                }
                if !in_xfers[i].is_null() {
                    usb_host_transfer_free(in_xfers[i]);
                }
                if claimed_intfs[i] >= 0 {
                    usb_host_interface_release(client, dev_hdl, claimed_intfs[i] as u8);
                }
                // Release CDC communication class interface (if claimed separately)
                if comm_intfs[i] >= 0 {
                    usb_host_interface_release(client, dev_hdl, comm_intfs[i] as u8);
                }
            }
            usb_host_device_close(client, dev_hdl);
            break;
        }
    }
}

// ─── UART driver helpers ──────────────────────────────────────────────────────

/// Initialise a UART port with 8N1 framing and the given baud rate / GPIO pins.
/// Returns `true` on success.
unsafe fn uart_init(port_num: i32, tx_pin: i32, rx_pin: i32, baud_rate: u32) -> bool {
    let cfg = uart_config_t {
        baud_rate: baud_rate as i32,
        data_bits: uart_word_length_t_UART_DATA_8_BITS,
        parity:    uart_parity_t_UART_PARITY_DISABLE,
        stop_bits: uart_stop_bits_t_UART_STOP_BITS_1,
        flow_ctrl: uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE,
        rx_flow_ctrl_thresh: 0,
        ..core::mem::zeroed()
    };
    let rc = uart_param_config(port_num as u32, &cfg);
    if rc != 0 {
        log::warn!("[UART{}] uart_param_config failed: {}", port_num, rc);
        return false;
    }
    // -1 = UART_PIN_NO_CHANGE; no RTS/CTS needed
    let rc = uart_set_pin(port_num as u32, tx_pin, rx_pin, -1, -1);
    if rc != 0 {
        log::warn!("[UART{}] uart_set_pin failed: {}", port_num, rc);
        return false;
    }
    // RX buffer 512 B, no TX buffer (direct write), no event queue
    let rc = uart_driver_install(port_num as u32, 512, 0, 0, core::ptr::null_mut(), 0);
    if rc != 0 {
        log::warn!("[UART{}] uart_driver_install failed: {}", port_num, rc);
        return false;
    }
    log::info!("[UART{}] init ok  tx={} rx={} baud={}", port_num, tx_pin, rx_pin, baud_rate);
    true
}

/// UART RX forwarding thread.
///
/// Reads from `port_num` with a 10-tick timeout.  When `ACTIVE_DEVICE ==
/// device_id`, the received bytes are forwarded to the SSH bridge ring buffer.
fn uart_rx_thread(port_num: i32, device_id: u8) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe {
            uart_read_bytes(
                port_num as u32,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                10,  // ticks; ~100 ms at 100 Hz FreeRTOS tick rate
            )
        };
        if n > 0 {
            // Forward to SSH bridge only when this is the active SSH device
            if ACTIVE_DEVICE.load(Ordering::Relaxed) == device_id {
                // log::info!("[RX] uart{} {} bytes", port_num, n);
                unsafe { ssh_bridge_cdc_rx(buf.as_ptr(), n as usize); }
            }
            // Mirror to per-port display buffer (always, regardless of page)
            let disp_lock = match device_id {
                DEVICE_COM1 => &COM1_DISP_BUF,
                DEVICE_COM2 => &COM2_DISP_BUF,
                _ => &COM1_DISP_BUF, // fallback
            };
            if let Some(ref disp_buf) = *disp_lock.lock().unwrap() {
                disp_buf.push_data(&buf[..n as usize]);
            }
            // Broadcast to WebSocket terminals
            ws_enqueue(device_id, &buf[..n as usize]);
        }
    }
}

// ─── Public status accessors ─────────────────────────────────────────────────

/// Returns (connected, vid, pid) for the currently attached USB CDC device.
pub fn cdc_device_info() -> (bool, u16, u16) {
    let connected = CDC_DEV_HDL.load(Ordering::Acquire) != 0;
    let vid = CDC_VID.load(Ordering::Acquire) as u16;
    let pid = CDC_PID.load(Ordering::Acquire) as u16;
    (connected, vid, pid)
}

/// Returns the chip name for the currently attached USB device based on VID/PID.
pub fn cdc_device_chip_name() -> &'static str {
    let vid = CDC_VID.load(Ordering::Acquire) as u16;
    let pid = CDC_PID.load(Ordering::Acquire) as u16;
    match (vid, pid) {
        (0x067B, 0x2303) | (0x067B, 0x23A3) | (0x067B, 0x2304) => "PL2303",
        (0x10C4, 0xEA60) | (0x10C4, 0xEA61) | (0x10C4, 0xEA70) => "CP210x",
        (0x0403, 0x6001) => "FT232R",
        (0x0403, 0x6010) => "FT2232",
        (0x0403, 0x6011) => "FT4232",
        (0x0403, 0x6014) => "FT232H",
        (0x1A86, 0x7523) | (0x1A86, 0x7522) => "CH340",
        (0x1A86, 0x55D4) | (0x1A86, 0x55D3) => "CH343",
        (0x1A86, 0x5523) => "CH341",
        (0x0483, 0x5740) => "STM32 VCP",
        (0x0525, 0xA4A7) => "Netchip CDC-ACM",
        (0x2341, _)      => "Arduino",
        (0x239A, _)      => "Adafruit",
        (0x303A, _)      => "Espressif",
        _                => "Unknown",
    }
}

/// Returns whether the USB CDC host is enabled.
pub fn cdc_is_enabled() -> bool {
    CDC_ENABLED.load(Ordering::Relaxed)
}

/// Returns the number of active USB CDC ports (0 when no device is connected).
pub fn cdc_port_count() -> u8 {
    CDC_PORT_COUNT.load(Ordering::Relaxed)
}

/// Returns the name of the currently active bridged device.
pub fn active_device_name() -> &'static str {
    match ACTIVE_DEVICE.load(Ordering::Relaxed) {
        DEVICE_USB0 => "usb0",
        DEVICE_USB1 => "usb1",
        DEVICE_USB2 => "usb2",
        DEVICE_USB3 => "usb3",
        DEVICE_COM1 => "com1",
        DEVICE_COM2 => "com2",
        _           => "none",
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn start(
    username: &'static str,
    password: &'static str,
    com1_tx: i32, com1_rx: i32, com1_baud: u32,
    com2_tx: i32, com2_rx: i32, com2_baud: u32,
    cdc_enable: bool,
    cdc_baud: u32,
    gpio_pwm: GpioPwmState,
) {
    // Store GPIO/PWM state so SSH callbacks can reach it.
    *GPIO_PWM_STATE.lock().unwrap() = Some(gpio_pwm);
    // Initialise UARTs and spawn their RX forwarding threads.
    if unsafe { uart_init(1, com1_tx, com1_rx, com1_baud) } {
        UART1_READY.store(true, Ordering::Relaxed);
        thread::Builder::new()
            .name("uart1_rx".into())
            .stack_size(8192)
            .spawn(move || uart_rx_thread(1, DEVICE_COM1))
            .expect("spawn uart1_rx");
    }
    if unsafe { uart_init(2, com2_tx, com2_rx, com2_baud) } {
        UART2_READY.store(true, Ordering::Relaxed);
        thread::Builder::new()
            .name("uart2_rx".into())
            .stack_size(8192)
            .spawn(move || uart_rx_thread(2, DEVICE_COM2))
            .expect("spawn uart2_rx");
    }

    // Initialise the SSH bridge (listening on port 22).
    if !SSH_BRIDGE_STARTED.swap(true, Ordering::SeqCst) {
        log::info!("[SSH] Initialising SSH bridge...");
        let user_cstr = std::ffi::CString::new(username)
            .unwrap_or_else(|_| std::ffi::CString::new("admin").unwrap());
        let pass_cstr = std::ffi::CString::new(password)
            .unwrap_or_else(|_| std::ffi::CString::new("esp32").unwrap());
        unsafe {
            let rc = ssh_bridge_init(22, user_cstr.as_ptr(), pass_cstr.as_ptr());
            log::info!("[SSH] SSH bridge init returned rc={}", rc);
        }
        // The C code stores the raw pointer; keep the memory alive forever.
        std::mem::forget(user_cstr);
        std::mem::forget(pass_cstr);
    }

    if cdc_enable {
        CDC_ENABLED.store(true, Ordering::SeqCst);
        CDC_BAUD.store(cdc_baud, Ordering::SeqCst);
        log::info!("[USB] CDC host enabled (baud={})", cdc_baud);

        // Log heap status before USB host starts
        unsafe {
            let free = esp_idf_sys::esp_get_free_heap_size();
            let min  = esp_idf_sys::esp_get_minimum_free_heap_size();
            log::info!("[HEAP] before USB host: free={} min={}", free, min);
        }

        thread::Builder::new()
            .name("usb_daemon".into())
            .stack_size(4096)
            .spawn(usb_host_daemon)
            .expect("spawn usb_daemon");

        thread::Builder::new()
            .name("usb_client".into())
            .stack_size(4096)
            .spawn(usb_client_task)
            .expect("spawn usb_client");
    } else {
        log::info!("[USB] CDC host disabled by config");
    }
}

/// Debugging variant: initialise UARTs and SSH bridge, but do NOT start USB
/// host tasks.  Equivalent to `start(..., cdc_enable: false)`.
pub fn start_ssh_only(
    username: &'static str,
    password: &'static str,
    com1_tx: i32, com1_rx: i32, com1_baud: u32,
    com2_tx: i32, com2_rx: i32, com2_baud: u32,
) {
    start(username, password, com1_tx, com1_rx, com1_baud,
          com2_tx, com2_rx, com2_baud, false, 115200,
          crate::gpio_ctrl::GpioPwmState::new());
}
