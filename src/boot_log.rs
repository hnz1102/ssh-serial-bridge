// Boot log — saves reset reason and boot count to NVS namespace "boot_log".
// Keeps a rolling log of the last 10 entries (newest first).
// SPDX-License-Identifier: MIT

use std::ffi::CString;
use esp_idf_sys::*;

const NVS_NS: &str = "boot_log";
const MAX_ENTRIES: usize = 10;

/// Human-readable label for each ESP reset reason.
pub fn reset_reason_str(reason: esp_reset_reason_t) -> &'static str {
    #[allow(non_upper_case_globals)]
    match reason {
        esp_reset_reason_t_ESP_RST_POWERON   => "PowerOn",
        esp_reset_reason_t_ESP_RST_EXT       => "ExtReset",
        esp_reset_reason_t_ESP_RST_SW        => "Software",
        esp_reset_reason_t_ESP_RST_PANIC     => "Panic/Crash",
        esp_reset_reason_t_ESP_RST_INT_WDT   => "IntWatchdog",
        esp_reset_reason_t_ESP_RST_TASK_WDT  => "TaskWatchdog",
        esp_reset_reason_t_ESP_RST_WDT       => "Watchdog",
        esp_reset_reason_t_ESP_RST_DEEPSLEEP => "DeepSleep",
        esp_reset_reason_t_ESP_RST_BROWNOUT  => "Brownout",
        esp_reset_reason_t_ESP_RST_SDIO      => "SDIO",
        _                                    => "Unknown",
    }
}

// ─── NVS helpers (scoped to boot_log namespace) ───────────────────────────────

fn nvs_open_rw() -> Option<nvs_handle_t> {
    let ns = CString::new(NVS_NS).ok()?;
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) == ESP_OK {
            Some(handle)
        } else {
            None
        }
    }
}

fn nvs_read_u32(handle: nvs_handle_t, key: &str) -> u32 {
    let k = match CString::new(key) { Ok(k) => k, Err(_) => return 0 };
    unsafe {
        let mut val: u32 = 0;
        nvs_get_u32(handle, k.as_ptr(), &mut val);
        val
    }
}

fn nvs_write_u32(handle: nvs_handle_t, key: &str, val: u32) {
    if let Ok(k) = CString::new(key) {
        unsafe { nvs_set_u32(handle, k.as_ptr(), val); }
    }
}

fn nvs_read_str_h(handle: nvs_handle_t, key: &str) -> Option<String> {
    let k = CString::new(key).ok()?;
    unsafe {
        let mut len: usize = 0;
        let ret = nvs_get_str(handle, k.as_ptr(), core::ptr::null_mut::<u8>(), &mut len);
        if ret != ESP_OK || len == 0 { return None; }
        let mut buf = vec![0u8; len];
        if nvs_get_str(handle, k.as_ptr(), buf.as_mut_ptr() as *mut u8, &mut len) != ESP_OK {
            return None;
        }
        while buf.last() == Some(&0) { buf.pop(); }
        String::from_utf8(buf).ok()
    }
}

fn nvs_write_str_h(handle: nvs_handle_t, key: &str, val: &str) {
    if let (Ok(k), Ok(v)) = (CString::new(key), CString::new(val)) {
        unsafe { nvs_set_str(handle, k.as_ptr(), v.as_ptr()); }
    }
}

fn prepend_entry(new: &str, existing: &str) -> String {
    let mut entries: Vec<&str> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().collect()
    };
    entries.insert(0, new);
    entries.truncate(MAX_ENTRIES);
    entries.join("\n")
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Call once at startup (after `nvs_flash_init`).
/// Reads the reset reason, increments the boot counter, and appends an entry to
/// the rolling log stored in NVS.
/// Returns `(boot_count, reason_str)`.
pub fn record_boot() -> (u32, &'static str) {
    let reason = unsafe { esp_reset_reason() };
    let reason_str = reset_reason_str(reason);

    let handle = match nvs_open_rw() {
        Some(h) => h,
        None => return (0, reason_str),
    };

    let count = nvs_read_u32(handle, "count").saturating_add(1);
    nvs_write_u32(handle, "count", count);

    let existing = nvs_read_str_h(handle, "log").unwrap_or_default();
    let new_entry = format!("#{} {}", count, reason_str);
    let updated = prepend_entry(&new_entry, &existing);
    nvs_write_str_h(handle, "log", &updated);

    unsafe {
        nvs_commit(handle);
        nvs_close(handle);
    }

    (count, reason_str)
}

/// Returns the boot log (up to last 10 entries, newest first) as a
/// newline-separated string.  Returns an empty string if NVS is unavailable.
pub fn read_log() -> String {
    let ns = match CString::new(NVS_NS) { Ok(s) => s, Err(_) => return String::new() };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return String::new();
        }
        let log = nvs_read_str_h(handle, "log").unwrap_or_default();
        nvs_close(handle);
        log
    }
}
