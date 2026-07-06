// HTTP Config Server for SSH-Serial-Bridge
// Web UI for device configuration with NVS persistence
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Hiroshi Nakajima

use log::*;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use esp_idf_svc::io::{Read, Write};
use embedded_svc::http::Headers;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_sys::*;
use std::ffi::CString;
use crate::gpio_ctrl::GpioPwmState;

// ─── App version ─────────────────────────────────────────────────────────────

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ─── NVS namespace ────────────────────────────────────────────────────────────

const NVS_NS: &str = "usbotg_cfg";

// ─── Config struct ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct NvsConfig {
    pub wifi_ssid:     String,
    pub wifi_psk:      String,
    pub ip_mode:       String,
    pub ip_address:    String,
    pub subnet_mask:   String,
    pub gateway:       String,
    pub dns:           String,
    pub syslog_server: String,
    pub syslog_enable: String,
    pub syslog_host_name: String,
    pub syslog_app_name: String,
    pub ssh_user:      String,
    pub ssh_password:  String,
    pub com1_tx_pin:   String,
    pub com1_rx_pin:   String,
    pub com1_baud:     String,
    pub com2_tx_pin:   String,
    pub com2_rx_pin:   String,
    pub com2_baud:     String,
    pub cdc_enable:    String,
    pub cdc_baud:      String,
    pub cdc_retry_enable: String,
    pub cdc_retry_interval: String,
    pub display_enable: String,
    pub display_port:  String,
    pub mini_display_enable: String,
    pub wps_enable:    String,
    pub pwm_enable:    String,
    pub ntp_server1:   String,
    pub ntp_server2:   String,
    pub ntp_server3:   String,
    pub ntp_server4:   String,
}

// ─── NVS helpers ─────────────────────────────────────────────────────────────

fn nvs_read(key: &str) -> Option<String> {
    let ns = CString::new(NVS_NS).ok()?;
    let k  = CString::new(key).ok()?;
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        // Get required length (pass null for out_value)
        let mut len: usize = 0;
        let ret = nvs_get_str(handle, k.as_ptr(), core::ptr::null_mut::<u8>(), &mut len);
        if ret != ESP_OK || len == 0 {
            nvs_close(handle);
            return None;
        }
        let mut buf = vec![0u8; len];
        let ret = nvs_get_str(handle, k.as_ptr(), buf.as_mut_ptr() as *mut u8, &mut len);
        nvs_close(handle);
        if ret != ESP_OK {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }
}

fn nvs_write_all(cfg: &NvsConfig) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let pairs: &[(&str, &str)] = &[
            ("wifi_ssid",     &cfg.wifi_ssid),
            ("wifi_psk",      &cfg.wifi_psk),
            ("ip_mode",       &cfg.ip_mode),
            ("ip_address",    &cfg.ip_address),
            ("subnet_mask",   &cfg.subnet_mask),
            ("gateway",       &cfg.gateway),
            ("dns",           &cfg.dns),
            ("syslog_server", &cfg.syslog_server),
            ("syslog_enable", &cfg.syslog_enable),
            ("syslog_hostname",    &cfg.syslog_host_name),
            ("syslog_app_name", &cfg.syslog_app_name),
            ("ssh_user",      &cfg.ssh_user),
            ("ssh_password",  &cfg.ssh_password),
            ("com1_tx_pin",   &cfg.com1_tx_pin),
            ("com1_rx_pin",   &cfg.com1_rx_pin),
            ("com1_baud",     &cfg.com1_baud),
            ("com2_tx_pin",   &cfg.com2_tx_pin),
            ("com2_rx_pin",   &cfg.com2_rx_pin),
            ("com2_baud",     &cfg.com2_baud),
            ("cdc_enable",    &cfg.cdc_enable),
            ("cdc_baud",      &cfg.cdc_baud),
            ("display_enable", &cfg.display_enable),
            ("display_port",  &cfg.display_port),
            ("wps_enable",    &cfg.wps_enable),
            ("pwm_enable",    &cfg.pwm_enable),
            ("ntp_server1",   &cfg.ntp_server1),
            ("ntp_server2",   &cfg.ntp_server2),
            ("ntp_server3",   &cfg.ntp_server3),
            ("ntp_server4",   &cfg.ntp_server4),
        ];
        let mut ok = true;
        for (key, val) in pairs {
            match (CString::new(*key), CString::new(*val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => {
                    warn!("NVS: CString conversion failed for key '{}'", key);
                    ok = false;
                }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

fn nvs_erase_namespace() -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let ok = nvs_erase_all(handle) == ESP_OK;
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Write only wifi_ssid and wifi_psk to NVS (called after a successful WPS handshake).
pub fn nvs_write_wifi_creds(ssid: &str, psk: &str) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let mut ok = true;
        for (key, val) in [("wifi_ssid", ssid), ("wifi_psk", psk)] {
            match (CString::new(key), CString::new(val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => { ok = false; }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Erase all NVS settings and restart the device (returns to cfg.toml defaults on next boot).
pub fn factory_reset() -> ! {
    info!("Factory reset: erasing NVS namespace '{}'...", NVS_NS);
    nvs_erase_namespace();
    info!("NVS erased. Restarting device...");
    unsafe { esp_idf_sys::esp_restart() }
}

/// Load config from NVS, falling back to the provided defaults for any missing keys.
pub fn load_config(defaults: NvsConfig) -> NvsConfig {
    fn or(nvs_val: Option<String>, default: &str) -> String {
        match nvs_val {
            Some(s) if !s.is_empty() => s,
            _ => default.to_string(),
        }
    }
    info!("Loading config from NVS (namespace: {})", NVS_NS);
    NvsConfig {
        wifi_ssid:     or(nvs_read("wifi_ssid"),     &defaults.wifi_ssid),
        wifi_psk:      or(nvs_read("wifi_psk"),      &defaults.wifi_psk),
        ip_mode:       or(nvs_read("ip_mode"),       &defaults.ip_mode),
        ip_address:    or(nvs_read("ip_address"),    &defaults.ip_address),
        subnet_mask:   or(nvs_read("subnet_mask"),   &defaults.subnet_mask),
        gateway:       or(nvs_read("gateway"),       &defaults.gateway),
        dns:           or(nvs_read("dns"),           &defaults.dns),
        syslog_server: or(nvs_read("syslog_server"), &defaults.syslog_server),
        syslog_enable: or(nvs_read("syslog_enable"), &defaults.syslog_enable),
        syslog_host_name: or(nvs_read("syslog_hostname"), &defaults.syslog_host_name),
        syslog_app_name: or(nvs_read("syslog_app_name"), &defaults.syslog_app_name),
        ssh_user:      or(nvs_read("ssh_user"),      &defaults.ssh_user),
        ssh_password:  or(nvs_read("ssh_password"),  &defaults.ssh_password),
        com1_tx_pin:   or(nvs_read("com1_tx_pin"),   &defaults.com1_tx_pin),
        com1_rx_pin:   or(nvs_read("com1_rx_pin"),   &defaults.com1_rx_pin),
        com1_baud:     or(nvs_read("com1_baud"),     &defaults.com1_baud),
        com2_tx_pin:   or(nvs_read("com2_tx_pin"),   &defaults.com2_tx_pin),
        com2_rx_pin:   or(nvs_read("com2_rx_pin"),   &defaults.com2_rx_pin),
        com2_baud:     or(nvs_read("com2_baud"),     &defaults.com2_baud),
        cdc_enable:    or(nvs_read("cdc_enable"),    &defaults.cdc_enable),
        cdc_baud:      or(nvs_read("cdc_baud"),      &defaults.cdc_baud),
        cdc_retry_enable: or(nvs_read("cdc_retry_enable"), &defaults.cdc_retry_enable),
        cdc_retry_interval: or(nvs_read("cdc_retry_interval"), &defaults.cdc_retry_interval),
        display_enable: or(nvs_read("display_enable"), &defaults.display_enable),
        display_port:  or(nvs_read("display_port"),  &defaults.display_port),
        mini_display_enable: or(nvs_read("mini_display_enable"), &defaults.mini_display_enable),
        wps_enable:    or(nvs_read("wps_enable"),    &defaults.wps_enable),
        pwm_enable:    or(nvs_read("pwm_enable"),    &defaults.pwm_enable),
        ntp_server1:   or(nvs_read("ntp_server1"),   &defaults.ntp_server1),
        ntp_server2:   or(nvs_read("ntp_server2"),   &defaults.ntp_server2),
        ntp_server3:   or(nvs_read("ntp_server3"),   &defaults.ntp_server3),
        ntp_server4:   or(nvs_read("ntp_server4"),   &defaults.ntp_server4),
    }
}

// ─── Session management ───────────────────────────────────────────────────────

fn generate_token() -> String {
    (0..4)
        .map(|_| format!("{:08x}", unsafe { esp_random() }))
        .collect()
}

fn get_session_from_cookie_header(cookie_header: Option<&str>) -> Option<String> {
    let h = cookie_header?;
    for part in h.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("session=") {
            return Some(v.to_string());
        }
    }
    None
}

fn is_auth(cookie_header: Option<&str>, session: &Arc<Mutex<Option<String>>>) -> bool {
    let guard = session.lock().unwrap();
    if let Some(token) = guard.as_ref() {
        return get_session_from_cookie_header(cookie_header)
            .as_deref() == Some(token.as_str());
    }
    false
}

// ─── URL decode / form body parse ────────────────────────────────────────────

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(c) = u8::from_str_radix(hex, 16) {
                    out.push(c as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn parse_form(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in body.split('&') {
        if let Some(eq) = pair.find('=') {
            map.insert(url_decode(&pair[..eq]), url_decode(&pair[eq + 1..]));
        }
    }
    map
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// Runtime status shared between the main loop and the HTTP server.
#[derive(Clone)]
pub struct StatusInfo {
    pub ip_address:     Arc<Mutex<String>>,
    pub ssid:           Arc<Mutex<String>>,
    pub rssi:           Arc<Mutex<i32>>,
    pub dc_in_voltage:  Arc<Mutex<f32>>,
    pub dc_out_voltage: Arc<Mutex<f32>>,
    pub chip_temp:      Arc<Mutex<f32>>,
}

impl StatusInfo {
    pub fn new() -> Self {
        Self {
            ip_address:     Arc::new(Mutex::new("--".to_string())),
            ssid:           Arc::new(Mutex::new("--".to_string())),
            rssi:           Arc::new(Mutex::new(0)),
            dc_in_voltage:  Arc::new(Mutex::new(0.0)),
            dc_out_voltage: Arc::new(Mutex::new(0.0)),
            chip_temp:      Arc::new(Mutex::new(0.0)),
        }
    }
    pub fn set_wifi(&self, ip: &str, ssid: &str, rssi: i32) {
        *self.ip_address.lock().unwrap() = ip.to_string();
        *self.ssid.lock().unwrap()       = ssid.to_string();
        *self.rssi.lock().unwrap()       = rssi;
    }
    pub fn set_rssi(&self, rssi: i32) {
        *self.rssi.lock().unwrap() = rssi;
    }
    pub fn set_voltages(&self, dc_in: f32, dc_out: f32) {
        *self.dc_in_voltage.lock().unwrap()  = dc_in;
        *self.dc_out_voltage.lock().unwrap() = dc_out;
    }
    pub fn set_chip_temp(&self, temp: f32) {
        *self.chip_temp.lock().unwrap() = temp;
    }
}

#[derive(Clone)]
pub struct ConfigState {
    pub config:   Arc<Mutex<NvsConfig>>,
    pub defaults: Arc<NvsConfig>,
    pub status:   StatusInfo,
    pub gpio_pwm: GpioPwmState,
    session:      Arc<Mutex<Option<String>>>,
}

impl ConfigState {
    pub fn new(config: NvsConfig, defaults: NvsConfig, status: StatusInfo, gpio_pwm: GpioPwmState) -> Self {
        Self {
            config:   Arc::new(Mutex::new(config)),
            defaults: Arc::new(defaults),
            status,
            gpio_pwm,
            session:  Arc::new(Mutex::new(None)),
        }
    }
}

// ─── HTML helpers ─────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[allow(dead_code)]
fn sel(current: &str, value: &str) -> &'static str {
    if current == value { "selected" } else { "" }
}

fn chk(current: &str, value: &str) -> &'static str {
    if current == value { "checked" } else { "" }
}

// ─── Login page ───────────────────────────────────────────────────────────────

const LOGIN_HTML: &str = concat!(
    r#"<!DOCTYPE html><html lang="ja"><head>"#,
    r#"<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">"#,
    r#"<title>SSH-Serial-Bridge Login</title><style>"#,
    r#"*{box-sizing:border-box}"#,
    r#"body{font-family:Arial,sans-serif;background:#1a1a2e;display:flex;justify-content:center;"#,
    r#"align-items:center;height:100vh;margin:0}"#,
    r#".box{background:#16213e;padding:40px;border-radius:12px;"#,
    r#"box-shadow:0 4px 20px rgba(0,0,0,.5);width:320px}"#,
    r#"h1{color:#e94560;text-align:center;margin:0 0 24px;font-size:1.4em}"#,
    r#"label{color:#a8b2d8;font-size:.9em;display:block;margin:12px 0 4px}"#,
    r#"input{width:100%;padding:10px;border:1px solid #304070;border-radius:6px;"#,
    r#"background:#0f3460;color:#e2e8f0;font-size:1em}"#,
    r#"input:focus{outline:none;border-color:#e94560}"#,
    r#"button{width:100%;margin-top:20px;padding:12px;background:#e94560;color:#fff;"#,
    r#"border:none;border-radius:6px;font-size:1em;font-weight:bold;cursor:pointer}"#,
    r#"button:hover{background:#c73652}"#,
    r#"#err{color:#f87171;text-align:center;margin-top:12px;font-size:.9em;display:none}"#,
    r#"</style></head><body>"#,
    r#"<div class="box"><h1>SSH-Serial-Bridge Config</h1>"#,
    r#"<form method="POST" action="/login">"#,
    r#"<label>Username</label>"#,
    r#"<input type="text" name="username" autocomplete="username" required>"#,
    r#"<label>Password</label>"#,
    r#"<input type="password" name="password" autocomplete="current-password" required>"#,
    r#"<button type="submit">Login</button>"#,
    r#"</form>"#,
    r#"<div id="err">Username or password is incorrect</div></div>"#,
    r#"<script>if(location.search.includes('error=1'))"#,
    r#"document.getElementById('err').style.display='block';</script>"#,
    r#"</body></html>"#
);

// ─── Config page builder ──────────────────────────────────────────────────────

fn build_config_html(cfg: &NvsConfig) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>SSH-Serial-Bridge Config</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:'Segoe UI',Arial,sans-serif;background:#111827;color:#e2e8f0;min-height:100vh}}
.page{{max-width:780px;margin:0 auto;padding:16px}}
h1{{color:#f87171;text-align:center;font-size:1.4em;padding:18px 0 14px;letter-spacing:.05em}}
/* Section headers with icon strip */
.section-title{{display:flex;align-items:center;gap:8px;color:#93c5fd;font-size:.82em;
  font-weight:700;text-transform:uppercase;letter-spacing:.08em;
  margin:0 0 12px;padding-bottom:6px;border-bottom:1px solid #1e3a5f}}
/* Cards */
.card{{background:#1e2d45;border-radius:10px;padding:16px 18px;margin-bottom:10px;
  border:1px solid #243b55;box-shadow:0 2px 8px rgba(0,0,0,.35)}}
/* Grid: 2-col on wide, 1-col on narrow */
.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:10px 14px;margin-bottom:4px}}
.grid-3{{display:grid;grid-template-columns:repeat(3,1fr);gap:10px 14px}}
@media(max-width:520px){{.grid-3{{grid-template-columns:1fr 1fr}}}}
/* Field */
.field label{{font-size:.76em;font-weight:600;color:#94a3b8;display:block;
  margin-bottom:4px;letter-spacing:.03em}}
.field input,.field select{{
  width:100%;padding:8px 10px;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:6px;color:#e2e8f0;font-size:.9em;transition:border-color .2s}}
.field input:focus,.field select:focus{{outline:none;border-color:#60a5fa;
  box-shadow:0 0 0 2px rgba(96,165,250,.15)}}
/* Radio toggle group */
.toggle-group{{display:inline-flex;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:8px;overflow:hidden;margin-bottom:10px}}
.toggle-group label{{padding:7px 18px;font-size:.85em;font-weight:600;cursor:pointer;
  color:#94a3b8;transition:background .2s,color .2s;user-select:none}}
.toggle-group input[type=radio]{{display:none}}
.toggle-group input[type=radio]:checked+span{{background:#1d4ed8;color:#fff;
  border-radius:7px;display:block;padding:7px 18px;margin:-7px -18px}}
/* Indent block shown/hidden based on IP mode */
.indent{{padding-left:4px;border-left:2px solid #1d4ed8;margin-top:6px}}
/* Status rows */
.stat-grid{{display:grid;grid-template-columns:auto 1fr;gap:6px 16px;align-items:center}}
.stat-key{{font-size:.78em;font-weight:600;color:#64748b;text-transform:uppercase;
  letter-spacing:.06em;white-space:nowrap}}
.stat-val{{font-size:.9em;color:#7dd3fc;font-weight:600}}
.badge{{display:inline-block;padding:2px 9px;border-radius:20px;font-size:.8em;font-weight:700}}
.badge-ok{{background:#14532d;color:#4ade80;border:1px solid #166534}}
.badge-warn{{background:#451a03;color:#fb923c;border:1px solid #7c2d12}}
.badge-err{{background:#4c0519;color:#fb7185;border:1px solid #881337}}
/* Message bar */
#msg{{padding:10px 14px;border-radius:7px;margin-bottom:10px;font-size:.88em;display:none;
  border-left:4px solid transparent}}
#msg.ok{{background:#052e16;border-color:#16a34a;color:#4ade80}}
#msg.err{{background:#350005;border-color:#dc2626;color:#fca5a5}}
/* Action buttons */
.actions{{display:flex;flex-wrap:wrap;gap:8px}}
.btn{{flex:1;min-width:110px;padding:11px 8px;border:none;border-radius:7px;
  font-size:.9em;font-weight:700;cursor:pointer;letter-spacing:.02em;transition:filter .2s}}
.btn:hover{{filter:brightness(1.15)}}
.btn-save{{background:#1d4ed8;color:#fff}}
.btn-reset{{background:#b45309;color:#fff}}
.btn-reboot{{background:#be123c;color:#fff}}
.btn-gpio{{background:#0f766e;color:#fff;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px;border:none;cursor:pointer;letter-spacing:.02em}}
.btn-gpio:hover{{filter:brightness(1.15)}}
.btn-logout{{background:#374151;color:#e2e8f0;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px}}
/* Divider */
hr{{border:none;border-top:1px solid #1e3a5f;margin:10px 0}}
</style></head><body>
<div class="page">
<h1>&#x1F5A7; SSH-Serial-Bridge Config <span style="font-size:.6em;color:#94a3b8">v{version}</span></h1>
<div id="msg"></div>

<!-- Status -->
<div class="card">
  <div class="section-title">&#x1F4F6; Device Status</div>
  <div id="status-card" class="stat-grid">
    <span class="stat-key">Loading</span><span class="stat-val">...</span>
  </div>
</div>

<form id="cfg">

<!-- Actions -->
<div class="card">
  <div class="actions">
    <button type="button" class="btn btn-save" onclick="saveConfig()">&#x1F4BE; Save to NVS</button>
    <button type="button" class="btn btn-reset" onclick="resetDefaults()">&#x21BA; Reset Defaults</button>
    <button type="button" class="btn btn-reboot" onclick="reboot()">&#x1F501; Reboot</button>
    <a href="/gpio" class="btn-gpio">&#x26A1; GPIO/PWM</a>
    <a href="/terminal" class="btn-gpio" style="background:#7c3aed">&#x1F4BB; Terminal</a>
    <a href="/logout" class="btn-logout">&#x1F6AA; Logout</a>
    <button type="button" class="btn" style="background:#0e7490" onclick="showBootLog()">&#x1F4DC; Boot Log</button>
  </div>
</div>

<!-- Boot Log Modal -->
<div id="boot-log-overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,.7);z-index:100;align-items:center;justify-content:center">
  <div style="background:#1e2d45;border:1px solid #243b55;border-radius:12px;padding:20px 24px;max-width:480px;width:90%;max-height:80vh;overflow-y:auto;box-shadow:0 8px 32px rgba(0,0,0,.6)">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:14px">
      <span style="color:#93c5fd;font-weight:700;font-size:.95em;text-transform:uppercase;letter-spacing:.08em">&#x1F4DC; Boot Log</span>
      <button onclick="closeBootLog()" style="background:none;border:none;color:#94a3b8;font-size:1.4em;cursor:pointer;line-height:1">&times;</button>
    </div>
    <div id="boot-log-body" style="font-size:.88em;color:#e2e8f0;line-height:1.7">
      <span style="color:#64748b">Loading...</span>
    </div>
  </div>
</div>

<!-- WiFi -->
<div class="card">
  <div class="section-title">&#x1F4F6; WiFi</div>
  <div class="grid">
    <div class="field"><label>SSID</label>
      <input type="text" name="wifi_ssid" value="{wifi_ssid}" maxlength="32" autocomplete="off"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="wifi_psk" value="{wifi_psk}" maxlength="64" autocomplete="off"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">WPS (PBC)</div>
  <div class="toggle-group">
    <label><input type="radio" name="wps_enable" value="true" {wps_true}><span>Enable</span></label>
    <label><input type="radio" name="wps_enable" value="false" {wps_false}><span>Disable</span></label>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.25em 0 0;">When WPS is enabled and SSID is empty, WPS PBC is triggered on next boot.</p>
</div>

<!-- Network -->
<div class="card">
  <div class="section-title">&#x1F310; Network</div>
  <div class="toggle-group">
    <label><input type="radio" name="ip_mode" value="dhcp" {dhcp_chk} onchange="ipModeChange()"><span>DHCP</span></label>
    <label><input type="radio" name="ip_mode" value="static" {static_chk} onchange="ipModeChange()"><span>Static IP</span></label>
  </div>
  <div class="indent" id="static-fields">
    <div class="grid">
      <div class="field"><label>IP Address</label>
        <input type="text" name="ip_address" value="{ip_address}" placeholder="192.168.1.100"></div>
      <div class="field"><label>Subnet Mask</label>
        <input type="text" name="subnet_mask" value="{subnet_mask}" placeholder="255.255.255.0"></div>
      <div class="field"><label>Gateway</label>
        <input type="text" name="gateway" value="{gateway}" placeholder="192.168.1.1"></div>
      <div class="field"><label>DNS Server</label>
        <input type="text" name="dns" value="{dns}" placeholder="192.168.1.1"></div>
    </div>
  </div>
</div>

<!-- Syslog -->
<div class="card">
  <div class="section-title">&#x1F4CB; Syslog</div>
  <div class="grid">
    <div class="field" style="grid-column:1/-1"><label>Server (host:port)</label>
      <input type="text" name="syslog_server" value="{syslog_server}" placeholder="192.168.1.1:514"></div>
    <div class="field"><label>Host Name</label>
      <input type="text" name="syslog_hostname" value="{syslog_host_name}" placeholder="esp32"></div>
    <div class="field"><label>App Name</label>
      <input type="text" name="syslog_app_name" value="{syslog_app_name}" placeholder="app"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">Enable Syslog</div>
  <div class="toggle-group">
    <label><input type="radio" name="syslog_enable" value="true" {syslog_true}><span>Enabled</span></label>
    <label><input type="radio" name="syslog_enable" value="false" {syslog_false}><span>Disabled</span></label>
  </div>
</div>

<!-- Auth -->
<div class="card">
  <div class="section-title">&#x1F510; Authentication (SSH &amp; Web)</div>
  <div class="grid">
    <div class="field"><label>Username</label>
      <input type="text" name="ssh_user" value="{ssh_user}" maxlength="64" autocomplete="username"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="ssh_password" value="{ssh_password}" maxlength="64" autocomplete="new-password"></div>
  </div>
</div>

<!-- Serial Ports -->
<div class="card">
  <div class="section-title">&#x1F50C; Serial Ports (UART)</div>
  <div style="font-size:.78em;font-weight:600;color:#475569;text-transform:uppercase;
    letter-spacing:.06em;margin-bottom:6px">COM1 / UART1</div>
  <div class="grid-3">
    <div class="field"><label>TX Pin</label>
      <input type="number" name="com1_tx_pin" value="{com1_tx_pin}" min="0" max="48"></div>
    <div class="field"><label>RX Pin</label>
      <input type="number" name="com1_rx_pin" value="{com1_rx_pin}" min="0" max="48"></div>
    <div class="field"><label>Baud Rate</label>
      <input type="number" name="com1_baud" value="{com1_baud}" min="300" max="921600"></div>
  </div>
  <hr>
  <div style="font-size:.78em;font-weight:600;color:#475569;text-transform:uppercase;
    letter-spacing:.06em;margin:8px 0 6px">COM2 / UART2</div>
  <div class="grid-3">
    <div class="field"><label>TX Pin</label>
      <input type="number" name="com2_tx_pin" value="{com2_tx_pin}" min="0" max="48"></div>
    <div class="field"><label>RX Pin</label>
      <input type="number" name="com2_rx_pin" value="{com2_rx_pin}" min="0" max="48"></div>
    <div class="field"><label>Baud Rate</label>
      <input type="number" name="com2_baud" value="{com2_baud}" min="300" max="921600"></div>
  </div>
</div>

<!-- USB CDC -->
<div class="card">
  <div class="section-title">&#x1F4BB; USB CDC Host</div>
  <div class="toggle-group">
    <label><input type="radio" name="cdc_enable" value="true" {cdc_true}><span>Enabled</span></label>
    <label><input type="radio" name="cdc_enable" value="false" {cdc_false}><span>Disabled</span></label>
  </div>
  <div class="grid-3">
    <div class="field"><label>Baud Rate</label>
      <input type="number" name="cdc_baud" value="{cdc_baud}" min="300" max="921600"></div>
  </div>
</div>

<!-- Display Port -->
<div class="card">
  <div class="section-title">&#x1F4FA; Display Serial Port</div>
  <div class="grid-3">
    <div class="field"><label>Monitor Port</label>
      <select name="display_port">
        <option value="com1" {dp_com1}>COM1</option>
        <option value="com2" {dp_com2}>COM2</option>
        <option value="usb0" {dp_usb0}>USB0</option>
      </select></div>
  </div>
</div>

<!-- PWM Control -->
<div class="card">
  <div class="section-title">&#x1F4CA; PWM Control (GPIO 10, 11)</div>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">Enable PWM Output</div>
  <div class="toggle-group">
    <label><input type="radio" name="pwm_enable" value="true" {pwm_true}><span>Enabled</span></label>
    <label><input type="radio" name="pwm_enable" value="false" {pwm_false}><span>Disabled</span></label>
  </div>
  <div style="font-size:.78em;color:#475569;margin-top:6px">Disabled: PWM pins are not initialized (useful when pins are used for other purposes)</div>
</div>

<!-- NTP Servers -->
<div class="card">
  <div class="section-title">&#x1F552; NTP Servers</div>
  <div class="grid">
    <div class="field"><label>NTP Server 1 (Primary)</label>
      <input type="text" name="ntp_server1" value="{ntp_server1}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 2</label>
      <input type="text" name="ntp_server2" value="{ntp_server2}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 3</label>
      <input type="text" name="ntp_server3" value="{ntp_server3}" placeholder="time.cloudflare.com"></div>
    <div class="field"><label>NTP Server 4</label>
      <input type="text" name="ntp_server4" value="{ntp_server4}" placeholder="ntp.nict.jp"></div>
  </div>
</div>


</form>
</div>
<script>
// Show/hide static IP fields based on ip_mode radio
function ipModeChange() {{
  var isStatic = document.querySelector('input[name=ip_mode]:checked').value === 'static';
  document.getElementById('static-fields').style.display = isStatic ? 'block' : 'none';
}}
// Apply on load
ipModeChange();
function showMsg(text, ok) {{
  var d = document.getElementById('msg');
  d.textContent = text;
  d.className = ok ? 'ok' : 'err';
  d.style.display = 'block';
  d.scrollIntoView({{behavior:'smooth'}});
}}
function saveConfig() {{
  var fd = new FormData(document.getElementById('cfg'));
  var body = new URLSearchParams(fd).toString();
  fetch('/api/config', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/x-www-form-urlencoded'}},
    body: body
  }})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Save failed: ' + e, false));
}}
function reboot() {{
  if (!confirm('Reboot the device?')) return;
  fetch('/api/reboot', {{method: 'POST'}})
  .then(() => {{
    showMsg('Rebooting... redirecting to login in 3 seconds.', true);
    setTimeout(() => {{ location.href = '/login'; }}, 3000);
  }})
  .catch(e => showMsg('Reboot request failed: ' + e, false));
}}
function resetDefaults() {{
  if (!confirm('Reset all settings to compile-time defaults and reload the page?')) return;
  fetch('/api/reset', {{method: 'POST'}})
  .then(r => r.text())
  .then(() => {{ location.reload(); }})
  .catch(e => showMsg('Reset failed: ' + e, false));
}}
function rssiBar(rssi) {{
  if (rssi >= -55) return '<span class="badge badge-ok">Excellent (' + rssi + ' dBm)</span>';
  if (rssi >= -70) return '<span class="badge badge-ok">Good (' + rssi + ' dBm)</span>';
  if (rssi >= -80) return '<span class="badge badge-warn">Fair (' + rssi + ' dBm)</span>';
  return '<span class="badge badge-err">Weak (' + rssi + ' dBm)</span>';
}}
function updateStatus() {{
  fetch('/api/status')
  .then(r => r.json())
  .then(d => {{
    var usb = d.cdc_enabled
      ? (d.usb_connected
          ? '<span class="badge badge-ok">&#x2705; ' + d.usb_device + ' (' + d.usb_vid + '/' + d.usb_pid + ')</span>'
          : '<span class="badge badge-warn">No device</span>')
      : '<span class="badge badge-err">Disabled</span>';
    var dev = d.active_device !== 'none'
      ? '<span class="badge badge-ok">' + d.active_device + '</span>'
      : '<span class="badge badge-warn">none</span>';
    document.getElementById('status-card').innerHTML =
      '<span class="stat-key">IP Address</span><span class="stat-val">' + d.ip + '</span>' +
      '<span class="stat-key">WiFi SSID</span><span class="stat-val">' + d.ssid + '</span>' +
      '<span class="stat-key">RSSI</span><span class="stat-val">' + rssiBar(d.rssi) + '</span>' +
      '<span class="stat-key">DC In Voltage</span><span class="stat-val"><span class="badge badge-ok">' + d.dc_in_voltage.toFixed(2) + ' V</span></span>' +
      '<span class="stat-key">DC Out Voltage</span><span class="stat-val"><span class="badge badge-ok">' + d.dc_out_voltage.toFixed(2) + ' V</span></span>' +
      '<span class="stat-key">Chip Temp</span><span class="stat-val"><span class="badge ' + (d.chip_temp < 70 ? 'badge-ok' : d.chip_temp < 85 ? 'badge-warn' : 'badge-err') + '">' + d.chip_temp.toFixed(1) + ' °C</span></span>' +
      '<span class="stat-key">USB CDC</span><span class="stat-val">' + usb + '</span>' +
      '<span class="stat-key">Active Device</span><span class="stat-val">' + dev + '</span>' +
      '<span class="stat-key">Display Port</span><span class="stat-val"><span class="badge badge-ok">' + d.display_port + '</span></span>' +
      '<span class="stat-key">Version</span><span class="stat-val"><span class="badge badge-ok">v' + d.version + '</span></span>';
  }})
  .catch(() => {{}});
}}
updateStatus();
setInterval(updateStatus, 3000);
function showBootLog() {{
  var overlay = document.getElementById('boot-log-overlay');
  overlay.style.display = 'flex';
  document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">Loading...</span>';
  fetch('/api/boot_log')
  .then(r => r.json())
  .then(d => {{
    if (!d.entries || d.entries.length === 0) {{
      document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">No entries yet.</span>';
      return;
    }}
    var reasons = {{
      'PowerOn':     ['badge-ok',   '&#x1F50C; PowerOn'],
      'Software':    ['badge-ok',   '&#x1F501; Software'],
      'Panic/Crash': ['badge-err',  '&#x1F4A5; Panic/Crash'],
      'IntWatchdog': ['badge-err',  '&#x231B; IntWatchdog'],
      'TaskWatchdog':['badge-warn', '&#x231B; TaskWatchdog'],
      'Watchdog':    ['badge-warn', '&#x231B; Watchdog'],
      'Brownout':    ['badge-err',  '&#x26A1; Brownout'],
      'DeepSleep':   ['badge-ok',   '&#x1F4A4; DeepSleep'],
      'ExtReset':    ['badge-ok',   '&#x1F504; ExtReset'],
      'SDIO':        ['badge-warn', '&#x1F4BE; SDIO'],
    }};
    var html = '<table style="width:100%;border-collapse:collapse">' +
      '<tr><th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55">#</th>' +
      '<th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55;padding-left:10px">Reset Reason</th></tr>';
    d.entries.forEach(function(e) {{
      var m = e.match(/^#(\d+)\s+(.+)$/);
      if (!m) return;
      var num = m[1], reason = m[2].trim();
      var info = reasons[reason] || ['badge-warn', reason];
      html += '<tr><td style="padding:5px 0;color:#64748b;font-size:.85em;vertical-align:top">' + num + '</td>' +
        '<td style="padding:5px 0 5px 10px"><span class="badge ' + info[0] + '">' + info[1] + '</span></td></tr>';
    }});
    html += '</table>';
    document.getElementById('boot-log-body').innerHTML = html;
  }})
  .catch(e => {{
    document.getElementById('boot-log-body').innerHTML = '<span style="color:#fca5a5">Failed to load: ' + e + '</span>';
  }});
}}
function closeBootLog() {{
  document.getElementById('boot-log-overlay').style.display = 'none';
}}
document.getElementById('boot-log-overlay').addEventListener('click', function(e) {{
  if (e.target === this) closeBootLog();
}});
</script>
</body></html>"#,
        wifi_ssid     = esc(&cfg.wifi_ssid),
        wifi_psk      = esc(&cfg.wifi_psk),
        wps_true      = chk(&cfg.wps_enable, "true"),
        wps_false     = chk(&cfg.wps_enable, "false"),
        dhcp_chk      = chk(&cfg.ip_mode, "dhcp"),
        static_chk    = chk(&cfg.ip_mode, "static"),
        ip_address    = esc(&cfg.ip_address),
        subnet_mask   = esc(&cfg.subnet_mask),
        gateway       = esc(&cfg.gateway),
        dns           = esc(&cfg.dns),
        syslog_server = esc(&cfg.syslog_server),
        syslog_true   = chk(&cfg.syslog_enable, "true"),
        syslog_false  = chk(&cfg.syslog_enable, "false"),
        syslog_host_name = esc(&cfg.syslog_host_name),
        syslog_app_name  = esc(&cfg.syslog_app_name),
        ssh_user      = esc(&cfg.ssh_user),
        ssh_password  = esc(&cfg.ssh_password),
        com1_tx_pin   = esc(&cfg.com1_tx_pin),
        com1_rx_pin   = esc(&cfg.com1_rx_pin),
        com1_baud     = esc(&cfg.com1_baud),
        com2_tx_pin   = esc(&cfg.com2_tx_pin),
        com2_rx_pin   = esc(&cfg.com2_rx_pin),
        com2_baud     = esc(&cfg.com2_baud),
        cdc_true      = chk(&cfg.cdc_enable, "true"),
        cdc_false     = chk(&cfg.cdc_enable, "false"),
        cdc_baud      = esc(&cfg.cdc_baud),
        dp_com1       = if cfg.display_port == "com1" { "selected" } else { "" },
        dp_com2       = if cfg.display_port == "com2" { "selected" } else { "" },
        dp_usb0       = if cfg.display_port == "usb0" { "selected" } else { "" },
        pwm_true      = chk(&cfg.pwm_enable, "true"),
        pwm_false     = chk(&cfg.pwm_enable, "false"),
        ntp_server1   = esc(&cfg.ntp_server1),
        ntp_server2   = esc(&cfg.ntp_server2),
        ntp_server3   = esc(&cfg.ntp_server3),
        ntp_server4   = esc(&cfg.ntp_server4),
        version       = APP_VERSION,
    )
}

// ─── GPIO/PWM control page ────────────────────────────────────────────────────

fn build_gpio_html(gpio_states: [bool; 7], pwm_duties: [u8; 2]) -> String {
    // GPIO 4-9: indices 0-5
    let gpio_rows: String = (0..6usize).map(|i| {
        let pin = i + 4;
        let checked = if gpio_states[i] { "checked" } else { "" };
        format!(
            "<tr><td class=\"pin-label\">GPIO {pin}</td>\
<td><label class=\"sw\"><input type=\"checkbox\" id=\"g{i}\" {checked} onchange=\"setGpio({i},this.checked)\">\
<span class=\"slider\"></span></label></td>\
<td><span id=\"gs{i}\" class=\"badge {cls}\">{lbl}</span></td></tr>",
            pin = pin,
            i = i,
            checked = checked,
            cls = if gpio_states[i] { "badge-ok" } else { "badge-err" },
            lbl = if gpio_states[i] { "ON" } else { "OFF" },
        )
    }).collect();

    // DCPOWER: index 6 = GPIO12
    let dc_checked = if gpio_states[6] { "checked" } else { "" };
    let dc_row = format!(
        "<tr><td class=\"pin-label\">DCPOWER</td>\
<td><label class=\"sw\"><input type=\"checkbox\" id=\"g6\" {checked} onchange=\"setGpio(6,this.checked)\">\
<span class=\"slider\"></span></label></td>\
<td><span id=\"gs6\" class=\"badge {cls}\">{lbl}</span></td></tr>",
        checked = dc_checked,
        cls = if gpio_states[6] { "badge-ok" } else { "badge-err" },
        lbl = if gpio_states[6] { "ON" } else { "OFF" },
    );

    let pwm_rows: String = (0..2usize).map(|i| {
        let pin = i + 10;
        let duty = pwm_duties[i];
        format!(
            "<tr><td class=\"pin-label\">GPIO {pin}</td>\
<td><input type=\"range\" min=\"0\" max=\"100\" value=\"{duty}\" class=\"range-input\"\
  id=\"p{i}\" oninput=\"updatePwmLabel({i},this.value)\" onchange=\"setPwm({i},this.value)\"></td>\
<td><span id=\"pl{i}\" class=\"duty-label\">{duty}%</span></td></tr>",
            pin = pin,
            i = i,
            duty = duty,
        )
    }).collect();

    format!(
        concat!(
            "<!DOCTYPE html><html lang=\"ja\"><head>\n",
            "<meta charset=\"UTF-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n",
            "<title>GPIO/PWM Control</title>\n",
            "<style>\n",
            "*{{box-sizing:border-box;margin:0;padding:0}}\n",
            "body{{font-family:'Segoe UI',Arial,sans-serif;background:#111827;color:#e2e8f0;min-height:100vh}}\n",
            ".page{{max-width:640px;margin:0 auto;padding:16px}}\n",
            "h1{{color:#f87171;text-align:center;font-size:1.4em;padding:18px 0 14px;letter-spacing:.05em}}\n",
            ".card{{background:#1e2d45;border-radius:10px;padding:16px 18px;margin-bottom:10px;",
              "border:1px solid #243b55;box-shadow:0 2px 8px rgba(0,0,0,.35)}}\n",
            ".section-title{{display:flex;align-items:center;gap:8px;color:#93c5fd;font-size:.82em;",
              "font-weight:700;text-transform:uppercase;letter-spacing:.08em;",
              "margin:0 0 12px;padding-bottom:6px;border-bottom:1px solid #1e3a5f}}\n",
            "table{{width:100%;border-collapse:collapse}}\n",
            "td{{padding:8px 6px;vertical-align:middle}}\n",
            ".pin-label{{font-size:.9em;font-weight:600;color:#94a3b8;width:90px}}\n",
            ".badge{{display:inline-block;padding:2px 9px;border-radius:20px;font-size:.8em;font-weight:700;width:42px;text-align:center}}\n",
            ".badge-ok{{background:#14532d;color:#4ade80;border:1px solid #166534}}\n",
            ".badge-err{{background:#4c0519;color:#fb7185;border:1px solid #881337}}\n",
            ".sw{{position:relative;display:inline-block;width:46px;height:26px}}\n",
            ".sw input{{display:none}}\n",
            ".slider{{position:absolute;cursor:pointer;top:0;left:0;right:0;bottom:0;",
              "background:#374151;border-radius:34px;transition:.3s}}\n",
            ".slider:before{{position:absolute;content:\"\";height:20px;width:20px;left:3px;bottom:3px;",
              "background:#e2e8f0;border-radius:50%;transition:.3s}}\n",
            ".sw input:checked+.slider{{background:#1d4ed8}}\n",
            ".sw input:checked+.slider:before{{transform:translateX(20px)}}\n",
            ".range-input{{-webkit-appearance:none;width:100%;height:6px;border-radius:3px;",
              "background:#374151;outline:none}}\n",
            ".range-input::-webkit-slider-thumb{{-webkit-appearance:none;width:18px;height:18px;",
              "border-radius:50%;background:#60a5fa;cursor:pointer}}\n",
            ".duty-label{{font-size:.95em;font-weight:700;color:#7dd3fc;min-width:40px;display:inline-block;text-align:right}}\n",
            ".back-link{{display:block;text-align:center;margin-top:12px;color:#60a5fa;font-size:.9em;text-decoration:none}}\n",
            ".back-link:hover{{text-decoration:underline}}\n",
            "</style></head><body>\n",
            "<div class=\"page\">\n",
            "<h1>&#x26A1; GPIO / PWM Control</h1>\n\n",
            "<div class=\"card\">\n",
            "  <div class=\"section-title\">&#x1F4A1; Digital Output (GPIO 4-9)</div>\n",
            "  <table>{gpio_rows}</table>\n",
            "</div>\n\n",
            "<div class=\"card\" style=\"border-color:#7c3aed\">\n",
            "  <div class=\"section-title\" style=\"color:#c4b5fd;border-color:#4c1d95\">&#x26A1; DC Power (GPIO 12)</div>\n",
            "  <table>{dc_row}</table>\n",
            "</div>\n\n",
            "<div class=\"card\">\n",
            "  <div class=\"section-title\">&#x1F4CA; PWM Output (GPIO 10-11)</div>\n",
            "  <table>{pwm_rows}</table>\n",
            "</div>\n\n",
            "<a class=\"back-link\" href=\"/\">&#x2190; Back to Config</a>\n",
            "</div>\n",
            "<script>\n",
            "function setGpio(idx,val){{\n",
            "  fetch('/api/gpio',{{method:'POST',headers:{{'Content-Type':'application/x-www-form-urlencoded'}},\n",
            "    body:'index='+idx+'&value='+(val?'1':'0')}}).then(r=>r.json()).then(d=>{{\n",
            "    var b=document.getElementById('gs'+idx);\n",
            "    b.textContent=d.gpio[idx]?'ON':'OFF';\n",
            "    b.className='badge '+(d.gpio[idx]?'badge-ok':'badge-err');\n",
            "    document.getElementById('g'+idx).checked=d.gpio[idx];\n",
            "  }}).catch(function(){{}});\n",
            "}}\n",
            "function updatePwmLabel(idx,val){{document.getElementById('pl'+idx).textContent=val+'%';}}\n",
            "function setPwm(idx,val){{\n",
            "  fetch('/api/pwm',{{method:'POST',headers:{{'Content-Type':'application/x-www-form-urlencoded'}},\n",
            "    body:'index='+idx+'&duty='+val}}).catch(function(){{}});\n",
            "}}\n",
            "function refreshState(){{\n",
            "  fetch('/api/gpio_state').then(r=>r.json()).then(d=>{{\n",
            "    for(var i=0;i<7;i++){{\n",
            "      var b=document.getElementById('gs'+i);\n",
            "      b.textContent=d.gpio[i]?'ON':'OFF';\n",
            "      b.className='badge '+(d.gpio[i]?'badge-ok':'badge-err');\n",
            "      document.getElementById('g'+i).checked=d.gpio[i];\n",
            "    }}\n",
            "    for(var j=0;j<2;j++){{\n",
            "      document.getElementById('p'+j).value=d.pwm[j];\n",
            "      document.getElementById('pl'+j).textContent=d.pwm[j]+'%';\n",
            "    }}\n",
            "  }}).catch(function(){{}});\n",
            "}}\n",
            "setInterval(refreshState,2000);\n",
            "</script>\n",
            "</body></html>"
        ),
        gpio_rows = gpio_rows,
        dc_row    = dc_row,
        pwm_rows  = pwm_rows,
    )
}

// ─── HTTP server ──────────────────────────────────────────────────────────────

pub fn start_http_server(state: ConfigState) -> anyhow::Result<EspHttpServer<'static>> {
    let config = Configuration::default();
    let mut server = EspHttpServer::new(&config)?;

    // ── GET /login ──────────────────────────────────────────────────────────
    server.fn_handler("/login", Method::Get, |request| {
        let mut response = request.into_ok_response()?;
        response.write_all(LOGIN_HTML.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /login ─────────────────────────────────────────────────────────
    let state_login = state.clone();
    server.fn_handler("/login", Method::Post, move |mut request| {
        let len = (request.content_len().unwrap_or(0) as usize).min(512);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let cfg = state_login.config.lock().unwrap();
        let valid = form.get("username").map(String::as_str) == Some(cfg.ssh_user.as_str())
            && form.get("password").map(String::as_str) == Some(cfg.ssh_password.as_str());
        drop(cfg);

        if valid {
            let token = generate_token();
            *state_login.session.lock().unwrap() = Some(token.clone());
            let cookie = format!(
                "session={}; Path=/; HttpOnly; SameSite=Strict",
                token
            );
            let headers = [("Set-Cookie", cookie.as_str()), ("Location", "/")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        } else {
            info!("HTTP: failed login attempt");
            let headers = [("Location", "/login?error=1")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /logout ─────────────────────────────────────────────────────────
    let state_logout = state.clone();
    server.fn_handler("/logout", Method::Get, move |request| {
        *state_logout.session.lock().unwrap() = None;
        let headers = [
            ("Set-Cookie", "session=; Path=/; Max-Age=0"),
            ("Location", "/login"),
        ];
        request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET / — config page (requires auth) ─────────────────────────────────
    let state_main = state.clone();
    server.fn_handler("/", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_main.session) {
            let headers = [("Location", "/login")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let cfg = state_main.config.lock().unwrap().clone();
        let html = build_config_html(&cfg);
        let headers = [("Content-Type", "text/html; charset=UTF-8")];
        let mut response = request.into_response(200, Some("OK"), &headers)?;
        response.write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/config — save config to NVS (requires auth) ───────────────
    let state_cfg = state.clone();
    server.fn_handler("/api/config", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_cfg.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(4096);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let mut cfg = state_cfg.config.lock().unwrap();
        macro_rules! update {
            ($field:ident, $key:expr) => {
                if let Some(v) = form.get($key) {
                    cfg.$field = v.clone();
                }
            };
        }
        update!(wifi_ssid,     "wifi_ssid");
        update!(wifi_psk,      "wifi_psk");
        update!(ip_mode,       "ip_mode");
        update!(ip_address,    "ip_address");
        update!(subnet_mask,   "subnet_mask");
        update!(gateway,       "gateway");
        update!(dns,           "dns");
        update!(syslog_server, "syslog_server");
        update!(syslog_enable, "syslog_enable");
        update!(syslog_host_name, "syslog_hostname");
        update!(syslog_app_name, "syslog_app_name");
        update!(ssh_user,      "ssh_user");
        update!(ssh_password,  "ssh_password");
        update!(com1_tx_pin,   "com1_tx_pin");
        update!(com1_rx_pin,   "com1_rx_pin");
        update!(com1_baud,     "com1_baud");
        update!(com2_tx_pin,   "com2_tx_pin");
        update!(com2_rx_pin,   "com2_rx_pin");
        update!(com2_baud,     "com2_baud");
        update!(cdc_enable,    "cdc_enable");
        update!(cdc_baud,      "cdc_baud");
        update!(display_port,  "display_port");
        update!(wps_enable,    "wps_enable");
        update!(pwm_enable,    "pwm_enable");
        update!(ntp_server1,   "ntp_server1");
        update!(ntp_server2,   "ntp_server2");
        update!(ntp_server3,   "ntp_server3");
        update!(ntp_server4,   "ntp_server4");

        // Apply display port change immediately (no reboot needed)
        crate::usb_host::set_display_port(&cfg.display_port);

        // Apply syslog hostname/app_name changes immediately (no reboot needed)
        if let Err(e) = crate::syslogger::update_logger_config(&cfg.syslog_host_name, &cfg.syslog_app_name) {
            warn!("Failed to update syslog config: {}", e);
        }

        let ok = nvs_write_all(&cfg);
        drop(cfg);

        if ok {
            info!("HTTP: config saved to NVS");
            let headers = [("Content-Type", "text/plain; charset=UTF-8")];
            request.into_response(200, Some("OK"), &headers)?
                .write_all("Settings saved. Reboot to apply.".as_bytes())?;
        } else {
            warn!("HTTP: failed to save config to NVS");
            request.into_status_response(500)?
                .write_all(b"NVS write error")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/status — device/wifi/usb status (requires auth) ────────────
    let state_stat = state.clone();
    server.fn_handler("/api/status", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_stat.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let ip   = state_stat.status.ip_address.lock().unwrap().clone();
        let ssid = state_stat.status.ssid.lock().unwrap().clone();
        let rssi = *state_stat.status.rssi.lock().unwrap();
        let dc_in  = *state_stat.status.dc_in_voltage.lock().unwrap();
        let dc_out = *state_stat.status.dc_out_voltage.lock().unwrap();
        let chip_temp = *state_stat.status.chip_temp.lock().unwrap();
        let (usb_connected, vid, pid) = crate::usb_host::cdc_device_info();
        let cdc_enabled  = crate::usb_host::cdc_is_enabled();
        let active_dev   = crate::usb_host::active_device_name();
        let chip_name    = crate::usb_host::cdc_device_chip_name();
        let usb_ports    = crate::usb_host::cdc_port_count();
        let json = format!(
            r#"{{"ip":"{ip}","ssid":"{ssid}","rssi":{rssi},"dc_in_voltage":{dc_in:.2},"dc_out_voltage":{dc_out:.2},"chip_temp":{chip_temp:.1},"cdc_enabled":{cdc_enabled},"usb_connected":{usb_connected},"usb_vid":"0x{vid:04X}","usb_pid":"0x{pid:04X}","usb_device":"{chip_name}","usb_ports":{usb_ports},"active_device":"{active_dev}","display_port":"{display_port}","version":"{version}"}}"#,
            ip          = esc(&ip),
            ssid        = esc(&ssid),
            rssi        = rssi,
            dc_in       = dc_in,
            dc_out      = dc_out,
            chip_temp   = chip_temp,
            cdc_enabled = cdc_enabled,
            usb_connected = usb_connected,
            vid         = vid,
            pid         = pid,
            chip_name   = chip_name,
            usb_ports   = usb_ports,
            active_dev  = active_dev,
            display_port = crate::usb_host::display_port_name(),
            version      = APP_VERSION,
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reset — erase NVS and restore defaults (requires auth) ────
    let state_rst = state.clone();
    server.fn_handler("/api/reset", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rst.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        nvs_erase_namespace();
        let defaults = (*state_rst.defaults).clone();
        *state_rst.config.lock().unwrap() = defaults;
        info!("HTTP: config reset to defaults");
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"Reset to defaults. NVS erased.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reboot (requires auth) ────────────────────────────────────
    let state_rb = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rb.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        // Clear session so the login page is shown after reconnect
        *state_rb.session.lock().unwrap() = None;
        request.into_ok_response()?.write_all(b"Rebooting...")?;
        // Use Builder with explicit stack size — bare std::thread::spawn can
        // silently fail on ESP-IDF when default stack is too small.
        let spawned = std::thread::Builder::new()
            .name("reboot".into())
            .stack_size(2048)
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe { esp_restart(); }
            });
        if spawned.is_err() {
            // Fallback: reboot directly if thread spawn failed
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe { esp_restart(); }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /gpio — GPIO/PWM control page (requires auth) ───────────────────
    let state_gpio_page = state.clone();
    server.fn_handler("/gpio", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_gpio_page.session) {
            let headers = [("Location", "/login")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let gpio_states = state_gpio_page.gpio_pwm.get_gpio();
        let pwm_duties  = state_gpio_page.gpio_pwm.get_pwm();
        let html = build_gpio_html(gpio_states, pwm_duties);
        let headers = [("Content-Type", "text/html; charset=UTF-8")];
        let mut response = request.into_response(200, Some("OK"), &headers)?;
        response.write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/gpio_state — returns current GPIO+PWM state as JSON ────────
    let state_gpio_get = state.clone();
    server.fn_handler("/api/gpio_state", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_gpio_get.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let g = state_gpio_get.gpio_pwm.get_gpio();
        let p = state_gpio_get.gpio_pwm.get_pwm();
        let json = format!(
            r#"{{"gpio":[{},{},{},{},{},{},{}],"pwm":[{},{}]}}"#,
            g[0] as u8, g[1] as u8, g[2] as u8, g[3] as u8, g[4] as u8, g[5] as u8, g[6] as u8,
            p[0], p[1],
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/gpio — set one GPIO output ON/OFF ──────────────────────────
    let state_gpio_set = state.clone();
    server.fn_handler("/api/gpio", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_gpio_set.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(64);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);
        if let (Some(idx_s), Some(val_s)) = (form.get("index"), form.get("value")) {
            if let Ok(idx) = idx_s.parse::<usize>() {
                let val = val_s == "1" || val_s == "true";
                state_gpio_set.gpio_pwm.set_gpio(idx, val);
                info!("HTTP: GPIO{} = {}", crate::gpio_ctrl::GPIO_PIN_MAP.get(idx).copied().unwrap_or(0), val);
            }
        }
        let g = state_gpio_set.gpio_pwm.get_gpio();
        let p = state_gpio_set.gpio_pwm.get_pwm();
        let json = format!(
            r#"{{"gpio":[{},{},{},{},{},{},{}],"pwm":[{},{}]}}"#,
            g[0] as u8, g[1] as u8, g[2] as u8, g[3] as u8, g[4] as u8, g[5] as u8, g[6] as u8,
            p[0], p[1],
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/pwm — set PWM duty for GPIO10 or GPIO11 ───────────────────
    let state_pwm_set = state.clone();
    server.fn_handler("/api/pwm", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_pwm_set.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(64);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);
        if let (Some(idx_s), Some(duty_s)) = (form.get("index"), form.get("duty")) {
            if let (Ok(idx), Ok(duty)) = (idx_s.parse::<usize>(), duty_s.parse::<u8>()) {
                state_pwm_set.gpio_pwm.set_pwm(idx, duty);
                info!("HTTP: PWM GPIO{} duty={}%", idx + 10, duty);
            }
        }
        let headers = [("Content-Type", "application/json")];
        let g = state_pwm_set.gpio_pwm.get_gpio();
        let p = state_pwm_set.gpio_pwm.get_pwm();
        let json = format!(
            r#"{{"gpio":[{},{},{},{},{},{}],"pwm":[{},{}]}}"#,
            g[0] as u8, g[1] as u8, g[2] as u8, g[3] as u8, g[4] as u8, g[5] as u8,
            p[0], p[1],
        );
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /terminal — serial terminal page (requires auth) ─────────────────
    let state_term = state.clone();
    server.fn_handler("/terminal", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_term.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_terminal_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm.js — serve embedded xterm.js ───────────────────────
    server.fn_handler("/static/xterm.js", Method::Get, |request| {
        let headers = [
            ("Content-Type", "application/javascript"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm.min.js"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm.css — serve embedded xterm.css ─────────────────────
    server.fn_handler("/static/xterm.css", Method::Get, |request| {
        let headers = [
            ("Content-Type", "text/css"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm.min.css"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm-addon-fit.js — serve embedded fit addon ───────────
    server.fn_handler("/static/xterm-addon-fit.js", Method::Get, |request| {
        let headers = [
            ("Content-Type", "application/javascript"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm-addon-fit.min.js"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/boot_log — boot history from NVS (requires auth) ───────────
    let state_bl = state.clone();
    server.fn_handler("/api/boot_log", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_bl.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let log = crate::boot_log::read_log();
        let entries: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
        let json_entries = entries.iter()
            .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!("{{\"entries\":[{}]}}", json_entries);
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── WebSocket /ws/serial — bidirectional serial bridge ──────────────────
    let _state_ws = state.clone();
    server.ws_handler("/ws/serial", move |ws| {
        // ── Connection open ─────────────────────────────────────────────
        if ws.is_new() {
            let fd = ws.session();
            // Port name will be sent as first text frame from client
            info!("[WS] new serial connection fd={}", fd);
            return Ok::<(), anyhow::Error>(());
        }
        // ── Connection closed ───────────────────────────────────────────
        if ws.is_closed() {
            let fd = ws.session();
            // Remove from all port sender lists
            crate::usb_host::ws_remove_sender("com1", fd);
            crate::usb_host::ws_remove_sender("com2", fd);
            crate::usb_host::ws_remove_sender("usb0", fd);
            crate::usb_host::ws_remove_sender("usb1", fd);
            crate::usb_host::ws_remove_sender("usb2", fd);
            crate::usb_host::ws_remove_sender("usb3", fd);
            info!("[WS] serial connection closed fd={}", fd);
            return Ok::<(), anyhow::Error>(());
        }
        // ── Verify auth via session cookie ──────────────────────────────
        // (WebSocket auth is checked once at open; data frames trust the connection)

        // ── Receive data ────────────────────────────────────────────────
        // Phase 1: get frame metadata
        let (frame_type, len) = ws.recv(&mut [])?;
        if len == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; len];
        ws.recv(&mut buf)?;

        match frame_type {
            embedded_svc::ws::FrameType::Text(_) => {
                // Text frame = control message (JSON): {"cmd":"init","port":"com1"}
                let msg = String::from_utf8_lossy(&buf);
                if let Some(port) = parse_ws_port(&msg) {
                    let fd = ws.session();
                    let sender = ws.create_detached_sender()?;
                    // Remove from previous port lists first
                    crate::usb_host::ws_remove_sender("com1", fd);
                    crate::usb_host::ws_remove_sender("com2", fd);
                    crate::usb_host::ws_remove_sender("usb0", fd);
                    crate::usb_host::ws_remove_sender("usb1", fd);
                    crate::usb_host::ws_remove_sender("usb2", fd);
                    crate::usb_host::ws_remove_sender("usb3", fd);
                    // Register for new port
                    crate::usb_host::ws_register_sender(&port, fd, sender);
                }
            }
            embedded_svc::ws::FrameType::Binary(_) => {
                // Binary frame = serial TX data from browser
                // Determine which port this fd is registered on
                let fd = ws.session();
                let port = ws_fd_port(fd);
                if !port.is_empty() {
                    crate::usb_host::serial_write(&port, &buf);
                }
            }
            _ => {}
        }
        Ok::<(), anyhow::Error>(())
    })?;

    info!("HTTP config server started on port 80");
    Ok(server)
}

// ─── WebSocket helpers ───────────────────────────────────────────────────────

/// Parse port name from WS init message like {"cmd":"init","port":"com1"}
fn parse_ws_port(msg: &str) -> Option<String> {
    // Simple parse — avoid pulling in serde_json
    if let Some(idx) = msg.find("\"port\"") {
        let rest = &msg[idx + 6..];
        if let Some(start) = rest.find('"') {
            let rest = &rest[start + 1..];
            if let Some(end) = rest.find('"') {
                let port = &rest[..end];
                match port {
                    "com1" | "com2" | "usb0" | "usb1" | "usb2" | "usb3" => return Some(port.to_string()),
                    "usb" => return Some("usb0".to_string()),
                    _ => {}
                }
            }
        }
    }
    None
}

/// Look up which port a WebSocket fd is registered on.
/// Delegates to ws_port_for_fd which uses WS_FD_PORT_MAP — a separate lock from
/// WS_SENDERS_* — to avoid deadlocking against the ws_send thread.
fn ws_fd_port(fd: i32) -> String {
    crate::usb_host::ws_port_for_fd(fd).to_string()
}

// ─── Terminal HTML page ──────────────────────────────────────────────────────

fn build_terminal_html() -> String {
    r#"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>SSH-Serial-Bridge Terminal</title>
<link rel="stylesheet" href="/static/xterm.css">
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;height:100vh;display:flex;flex-direction:column}
.toolbar{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;
  border-bottom:1px solid #243b55;flex-shrink:0}
.toolbar h1{font-size:1em;color:#93c5fd;letter-spacing:.05em;margin-right:auto}
.toolbar select,.toolbar button{padding:6px 14px;border-radius:6px;border:1px solid #2d4a6a;
  background:#0f172a;color:#e2e8f0;font-size:.85em;cursor:pointer}
.toolbar select:focus,.toolbar button:focus{outline:none;border-color:#60a5fa}
.toolbar button:hover{filter:brightness(1.2)}
.btn-connect{background:#166534;border-color:#16a34a;font-weight:700}
.btn-connect.connected{background:#991b1b;border-color:#dc2626}
.btn-back{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;
  align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.status{font-size:.78em;padding:3px 10px;border-radius:12px;font-weight:700}
.status.ok{background:#14532d;color:#4ade80}.status.off{background:#4c0519;color:#fb7185}
#terminal-container{flex:1;padding:4px;overflow:hidden}
</style></head><body>
<div class="toolbar">
  <h1>&#x1F4BB; Serial Terminal</h1>
  <label style="font-size:.8em;color:#94a3b8">Port:</label>
  <select id="port-select">
    <option value="com1">COM1</option>
    <option value="com2">COM2</option>
    <option value="usb0" id="opt-usb0" style="display:none">USB0</option>
    <option value="usb1" id="opt-usb1" style="display:none">USB1</option>
    <option value="usb2" id="opt-usb2" style="display:none">USB2</option>
    <option value="usb3" id="opt-usb3" style="display:none">USB3</option>
  </select>
  <button id="btn-conn" class="toolbar btn-connect" onclick="toggleConnection()">Connect</button>
  <span id="ws-status" class="status off">Disconnected</span>
  <a href="/" class="btn-back">&#x2190; Config</a>
</div>
<div id="terminal-container"></div>
<script src="/static/xterm.js"></script>
<script src="/static/xterm-addon-fit.js"></script>
<script>
const term = new Terminal({
  cursorBlink: true,
  fontSize: 14,
  theme: {
    background: '#0f172a',
    foreground: '#e2e8f0',
    cursor: '#60a5fa',
    selectionBackground: '#334155'
  }
});
const fitAddon = new FitAddon.FitAddon();
term.loadAddon(fitAddon);
term.open(document.getElementById('terminal-container'));
fitAddon.fit();
window.addEventListener('resize', () => fitAddon.fit());

let ws = null;
const portSel = document.getElementById('port-select');
const btnConn = document.getElementById('btn-conn');
const wsStatus = document.getElementById('ws-status');

function toggleConnection() {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.close();
  } else {
    connect();
  }
}

function connect() {
  const port = portSel.value;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(proto + '//' + location.host + '/ws/serial');
  ws.binaryType = 'arraybuffer';

  ws.onopen = () => {
    wsStatus.textContent = 'Connected (' + port.toUpperCase() + ')';
    wsStatus.className = 'status ok';
    btnConn.textContent = 'Disconnect';
    btnConn.classList.add('connected');
    portSel.disabled = true;
    // Send init message with port selection
    ws.send(JSON.stringify({cmd: 'init', port: port}));
    term.focus();
  };

  ws.onmessage = (ev) => {
    if (ev.data instanceof ArrayBuffer) {
      term.write(new Uint8Array(ev.data));
    } else {
      term.write(ev.data);
    }
  };

  ws.onclose = () => {
    wsStatus.textContent = 'Disconnected';
    wsStatus.className = 'status off';
    btnConn.textContent = 'Connect';
    btnConn.classList.remove('connected');
    portSel.disabled = false;
    ws = null;
  };

  ws.onerror = (err) => {
    console.error('WS error:', err);
    term.write('\r\n\x1b[31m[WebSocket error]\x1b[0m\r\n');
  };
}

// Terminal input → WebSocket (binary)
term.onData((data) => {
  if (ws && ws.readyState === WebSocket.OPEN) {
    const encoder = new TextEncoder();
    ws.send(encoder.encode(data));
  }
});

// Populate USB port options based on connected device
function updateUsbOptions() {
  fetch('/api/status')
  .then(r => r.json())
  .then(d => {
    var count = (d.cdc_enabled && d.usb_connected) ? (d.usb_ports || 1) : 0;
    for (var i = 0; i < 4; i++) {
      var opt = document.getElementById('opt-usb' + i);
      if (opt) opt.style.display = i < count ? '' : 'none';
    }
  })
  .catch(function(){});
}
updateUsbOptions();
setInterval(updateUsbOptions, 5000);
</script>
</body></html>"#.to_string()
}
