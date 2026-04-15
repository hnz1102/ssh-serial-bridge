// Wi-Fi connection and RSSI measurement
// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Hiroshi Nakajima

#![allow(dead_code)]

use std::time::Duration;
use std::thread;

use esp_idf_hal::peripheral;
use esp_idf_svc::{eventloop::EspSystemEventLoop, handle::RawHandle, wifi::EspWifi};
use esp_idf_sys;

use embedded_svc::wifi::{ClientConfiguration, Configuration};
use anyhow::bail;
use anyhow::Result;
use std::str::FromStr;

pub fn wifi_connect<'d> (
    modem: impl peripheral::Peripheral<P = esp_idf_hal::modem::Modem> + 'static,
    ssid: &'d str,
    pass: &'d str,
) -> Result<Box<EspWifi<'d>>> {

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