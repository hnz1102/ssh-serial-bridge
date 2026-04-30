// Wi-Fi connection and RSSI measurement
// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Hiroshi Nakajima

#![allow(dead_code)]

use std::time::Duration;
use std::thread;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use esp_idf_hal::peripheral;
use esp_idf_svc::{eventloop::EspSystemEventLoop, handle::RawHandle, wifi::EspWifi};
use esp_idf_sys;

use embedded_svc::wifi::{ClientConfiguration, Configuration};
use anyhow::bail;
use anyhow::Result;
use std::str::FromStr;
use log::info;

// ─── WPS state (shared between C event handler and Rust polling loop) ─────────

/// WPS credentials received from the AP (SSID, passphrase).
static WPS_CRED: Mutex<Option<(String, String)>> = Mutex::new(None);
/// Set to true when WPS fails or times out.
static WPS_FAILED: AtomicBool = AtomicBool::new(false);

/// Raw WPS event handler registered via esp_event_handler_register.
unsafe extern "C" fn wps_event_handler(
    _arg: *mut core::ffi::c_void,
    _base: esp_idf_sys::esp_event_base_t,
    event_id: i32,
    event_data: *mut core::ffi::c_void,
) {
    use esp_idf_sys::*;
    let id = event_id as u32;
    if id == wifi_event_t_WIFI_EVENT_STA_WPS_ER_SUCCESS {
        let ev = &*(event_data as *const wifi_event_sta_wps_er_success_t);
        let ssid_raw  = &ev.ap_cred[0].ssid;
        let pass_raw  = &ev.ap_cred[0].passphrase;
        let ssid_len  = ssid_raw.iter().position(|&b| b == 0).unwrap_or(ssid_raw.len());
        let pass_len  = pass_raw.iter().position(|&b| b == 0).unwrap_or(pass_raw.len());
        let ssid = String::from_utf8_lossy(&ssid_raw[..ssid_len]).into_owned();
        let pass = String::from_utf8_lossy(&pass_raw[..pass_len]).into_owned();
        if let Ok(mut c) = WPS_CRED.lock() {
            *c = Some((ssid, pass));
        }
    } else if id == wifi_event_t_WIFI_EVENT_STA_WPS_ER_FAILED
           || id == wifi_event_t_WIFI_EVENT_STA_WPS_ER_TIMEOUT {
        WPS_FAILED.store(true, Ordering::Release);
    }
}

// ─── Public API ───────────────────────────────────────────────────────────────

pub fn wifi_connect(
    modem: impl peripheral::Peripheral<P = esp_idf_hal::modem::Modem> + 'static,
    ssid: &str,
    pass: &str,
) -> Result<Box<EspWifi<'static>>> {

    if ssid.is_empty() || pass.is_empty() {
        bail!("SSID or password is empty");
    }
    let sys_event_loop = EspSystemEventLoop::take().unwrap();
    let mut wifi = Box::new(EspWifi::new(modem, sys_event_loop.clone(), None).unwrap());

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: heapless::String::<32>::from_str(ssid).unwrap(),
        password: heapless::String::<64>::from_str(pass).unwrap(),
        ..Default::default()
    })).unwrap();

    wifi.start().unwrap();
    wifi.connect()?;
    let mut timeout = 0;
    loop {
        if wifi.is_connected().unwrap(){
            // Disable power management to prevent beacon timeout issues
            unsafe {
                let ret = esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE);
                if ret == esp_idf_sys::ESP_OK as i32 {
                    info!("[WiFi] Power management disabled (WIFI_PS_NONE)");
                } else {
                    info!("[WiFi] Warning: Failed to disable WiFi PM: 0x{:x}", ret);
                }
            }
            // info!("Wifi connected");
            break;
        }
        thread::sleep(Duration::from_secs(1));
        timeout += 1;
        if timeout > 10 {
            // info!("Wifi could not be connected.");
            // wifi could not be connected, but we can use the wifi object to reconnect
            break;
        }
    }
    Ok(wifi)
}

pub fn configure_static_ip(
    wifi: &EspWifi,
    ip_addr: &str,
    subnet_mask: &str,
    gateway: &str,
    dns: &str,
) -> Result<()> {
    use std::net::Ipv4Addr;

    if ip_addr.is_empty() {
        bail!("Static IP address is not configured");
    }

    let ip: Ipv4Addr = ip_addr.parse().map_err(|e| anyhow::anyhow!("Invalid IP address '{}': {}", ip_addr, e))?;
    let mask: Ipv4Addr = subnet_mask.parse().map_err(|e| anyhow::anyhow!("Invalid subnet mask '{}': {}", subnet_mask, e))?;
    let gw: Ipv4Addr = gateway.parse().map_err(|e| anyhow::anyhow!("Invalid gateway '{}': {}", gateway, e))?;

    let netif = wifi.sta_netif();
    let handle = netif.handle();

    unsafe {
        // Stop DHCP client before setting static IP
        esp_idf_sys::esp_netif_dhcpc_stop(handle);

        let ip_info = esp_idf_sys::esp_netif_ip_info_t {
            ip: esp_idf_sys::esp_ip4_addr_t { addr: u32::from_ne_bytes(ip.octets()) },
            netmask: esp_idf_sys::esp_ip4_addr_t { addr: u32::from_ne_bytes(mask.octets()) },
            gw: esp_idf_sys::esp_ip4_addr_t { addr: u32::from_ne_bytes(gw.octets()) },
        };

        let ret = esp_idf_sys::esp_netif_set_ip_info(handle, &ip_info);
        if ret != 0 {
            bail!("esp_netif_set_ip_info failed: error code {}", ret);
        }

        // Set DNS server if provided
        if !dns.is_empty() {
            if let Ok(dns_ip) = dns.parse::<Ipv4Addr>() {
                let mut dns_info: esp_idf_sys::esp_netif_dns_info_t = core::mem::zeroed();
                dns_info.ip.u_addr.ip4.addr = u32::from_ne_bytes(dns_ip.octets());
                esp_idf_sys::esp_netif_set_dns_info(
                    handle,
                    esp_idf_sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
                    &mut dns_info,
                );
            }
        }
    }

    Ok(())
}

pub fn get_rssi() -> i32 {
    unsafe {
        let mut rssi : i32 = 0;
        esp_idf_sys::esp_wifi_sta_get_rssi(&mut rssi);
        rssi
    }
}

/// Connect to Wi-Fi using WPS PBC (Push Button Configuration).
///
/// Returns the connected `EspWifi` plus the SSID and passphrase obtained from
/// the AP so the caller can persist them to NVS.
///
/// The user must press the WPS button on the router within 120 seconds.
pub fn wifi_connect_wps(
    modem: impl peripheral::Peripheral<P = esp_idf_hal::modem::Modem> + 'static,
) -> Result<(Box<EspWifi<'static>>, String, String)> {
    info!("[WPS] Starting WPS PBC — press the WPS button on your router within 120 s");

    // Display WPS mode message
    crate::serial_display::show_system_message(
        "WPS MODE",
        "Press WPS button\non your router\nwithin 120 seconds"
    );

    let sys_event_loop = EspSystemEventLoop::take().unwrap();
    let mut wifi = Box::new(EspWifi::new(modem, sys_event_loop, None).unwrap());

    // STA mode with empty credentials
    wifi.set_configuration(&Configuration::Client(ClientConfiguration::default())).unwrap();
    wifi.start().unwrap();

    // Reset static WPS state
    WPS_FAILED.store(false, Ordering::Release);
    *WPS_CRED.lock().unwrap() = None;

    unsafe {
        // Register WPS event handler on the default system event loop
        esp_idf_sys::esp_event_handler_register(
            esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::ESP_EVENT_ANY_ID,
            Some(wps_event_handler),
            core::ptr::null_mut(),
        );

        // Enable WPS PBC and start the handshake
        let cfg = esp_idf_sys::esp_wps_config_t {
            wps_type: esp_idf_sys::wps_type_WPS_TYPE_PBC,
            ..Default::default()
        };
        let ret = esp_idf_sys::esp_wifi_wps_enable(&cfg);
        if ret != esp_idf_sys::ESP_OK as i32 {
            // Unregister event handler before returning
            esp_idf_sys::esp_event_handler_unregister(
                esp_idf_sys::WIFI_EVENT,
                esp_idf_sys::ESP_EVENT_ANY_ID,
                Some(wps_event_handler),
            );
            bail!("[WPS] esp_wifi_wps_enable failed: 0x{:x}", ret);
        }
        let ret = esp_idf_sys::esp_wifi_wps_start(0);
        if ret != esp_idf_sys::ESP_OK as i32 {
            esp_idf_sys::esp_wifi_wps_disable();
            // Unregister event handler before returning
            esp_idf_sys::esp_event_handler_unregister(
                esp_idf_sys::WIFI_EVENT,
                esp_idf_sys::ESP_EVENT_ANY_ID,
                Some(wps_event_handler),
            );
            bail!("[WPS] esp_wifi_wps_start failed: 0x{:x}", ret);
        }
    }

    // Poll for WPS result (up to 120 s)
    for elapsed in 0..120u32 {
        thread::sleep(Duration::from_secs(1));

        // Update countdown every 10 seconds
        if elapsed % 10 == 0 && elapsed > 0 {
            let remaining = 120 - elapsed;
            crate::serial_display::show_system_message(
                "WPS MODE",
                &format!("Press WPS button\non your router\n{} seconds remaining", remaining)
            );
        }

        if WPS_FAILED.load(Ordering::Acquire) {
            unsafe {
                esp_idf_sys::esp_wifi_wps_disable();
                // Unregister event handler
                esp_idf_sys::esp_event_handler_unregister(
                    esp_idf_sys::WIFI_EVENT,
                    esp_idf_sys::ESP_EVENT_ANY_ID,
                    Some(wps_event_handler),
                );
            }
            crate::serial_display::clear_system_message();
            bail!("[WPS] WPS failed or timed out by AP at {}s", elapsed);
        }

        if let Some((ssid, pass)) = WPS_CRED.lock().unwrap().clone() {
            unsafe {
                esp_idf_sys::esp_wifi_wps_disable();
                // Unregister event handler
                esp_idf_sys::esp_event_handler_unregister(
                    esp_idf_sys::WIFI_EVENT,
                    esp_idf_sys::ESP_EVENT_ANY_ID,
                    Some(wps_event_handler),
                );
            }
            info!("[WPS] Credentials received: SSID={}", ssid);

            // Show connecting message
            crate::serial_display::show_system_message(
                "WPS SUCCESS",
                &format!("Connecting to\n{}\nPlease wait...", ssid)
            );

            // Connect with the obtained credentials
            wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: heapless::String::<32>::from_str(&ssid)
                    .map_err(|_| anyhow::anyhow!("WPS SSID too long"))?,
                password: heapless::String::<64>::from_str(&pass)
                    .map_err(|_| anyhow::anyhow!("WPS passphrase too long"))?,
                ..Default::default()
            })).unwrap();
            wifi.connect()?;

            let mut t = 0;
            loop {
                if wifi.is_connected().unwrap() {
                    crate::serial_display::show_system_message(
                        "CONNECTED",
                        &format!("{}\nConnection successful!", ssid)
                    );
                    
                    // Wait for WPS internal processing to complete before changing power settings
                    info!("[WPS] Waiting for connection stabilization...");
                    thread::sleep(Duration::from_secs(3));
                    
                    // Disable power management to prevent beacon timeout issues after WPS
                    unsafe {
                        let ret = esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE);
                        if ret == esp_idf_sys::ESP_OK as i32 {
                            info!("[WPS] WiFi power management disabled (WIFI_PS_NONE)");
                        } else {
                            info!("[WPS] Warning: Failed to disable WiFi PM: 0x{:x}", ret);
                        }
                    }
                    
                    // Clear system message to return to normal display
                    crate::serial_display::clear_system_message();
                    break;
                }
                thread::sleep(Duration::from_secs(1));
                t += 1;
                if t >= 15 { break; }
            }

            return Ok((wifi, ssid, pass));
        }
    }

    unsafe {
        esp_idf_sys::esp_wifi_wps_disable();
        // Unregister event handler on timeout
        esp_idf_sys::esp_event_handler_unregister(
            esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::ESP_EVENT_ANY_ID,
            Some(wps_event_handler),
        );
    }
    crate::serial_display::clear_system_message();
    bail!("[WPS] WPS timed out after 120 s");
}