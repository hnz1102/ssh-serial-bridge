use log::info;
use esp_idf_svc::hal::ledc::LedcDriver;
use esp_idf_svc::hal::ledc::LedcTimerDriver;
use esp_idf_svc::hal::ledc::config::TimerConfig;
use esp_idf_svc::wifi::EspWifi;
use esp_idf_svc::sntp::{EspSntp, SyncStatus, SntpConf, OperatingMode, SyncMode};
use chrono::{DateTime, Utc};
use esp_idf_hal::{gpio::*, prelude::*};
use esp_idf_hal::adc::attenuation::DB_11;
use esp_idf_hal::adc::oneshot::{AdcDriver, AdcChannelDriver};
use esp_idf_hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_hal::temp_sensor::{TempSensorConfig, TempSensorDriver};
use std::time::{Duration, SystemTime};
use std::thread;

mod wifi;
mod syslogger;
pub mod usb_host;
mod httpserver;
mod gpio_ctrl;
pub mod serial_display;
mod btn_ctrl;
mod boot_log;

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("dhcp")]
    ip_mode: &'static str,
    #[default("")]
    ip_address: &'static str,
    #[default("255.255.255.0")]
    subnet_mask: &'static str,
    #[default("")]
    gateway: &'static str,
    #[default("")]
    dns: &'static str,
    #[default("")]
    syslog_server: &'static str,
    #[default("false")]
    syslog_enable: &'static str,
    #[default("esp32")]
    syslog_host_name: &'static str,
    #[default("app")]
    syslog_app_name: &'static str,
    #[default("admin")]
    ssh_user: &'static str,
    #[default("esp32")]
    ssh_password: &'static str,
    #[default("17")]
    com1_tx_pin: &'static str,
    #[default("18")]
    com1_rx_pin: &'static str,
    #[default("115200")]
    com1_baud: &'static str,
    #[default("19")]
    com2_tx_pin: &'static str,
    #[default("20")]
    com2_rx_pin: &'static str,
    #[default("115200")]
    com2_baud: &'static str,
    #[default("true")]
    cdc_enable: &'static str,
    #[default("115200")]
    cdc_baud: &'static str,
    #[default("true")]
    display_enable: &'static str,
    #[default("com1")]
    display_port: &'static str,
}

fn main() {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Peripherals Initialize
    let peripherals = Peripherals::take().unwrap();

    // ── Serial display on ST7789V (SPI3, 240x320) ─────────────────────────
    // Display init is deferred until after NVS config is loaded (see below)

    // NVS flash init (required before WiFi for RF calibration data)
    unsafe { esp_idf_sys::nvs_flash_init(); }
    // Record reset reason and boot count to NVS before anything else
    let (boot_count, reset_reason) = boot_log::record_boot();
    println!("Boot #{}, reset reason: {}", boot_count, reset_reason);
    // Load config from NVS (overrides cfg.toml defaults where NVS keys are set)
    let cfg_defaults = httpserver::NvsConfig {
        wifi_ssid:     CONFIG.wifi_ssid.to_string(),
        wifi_psk:      CONFIG.wifi_psk.to_string(),
        ip_mode:       CONFIG.ip_mode.to_string(),
        ip_address:    CONFIG.ip_address.to_string(),
        subnet_mask:   CONFIG.subnet_mask.to_string(),
        gateway:       CONFIG.gateway.to_string(),
        dns:           CONFIG.dns.to_string(),
        syslog_server: CONFIG.syslog_server.to_string(),
        syslog_enable: CONFIG.syslog_enable.to_string(),
        syslog_host_name: CONFIG.syslog_host_name.to_string(),
        syslog_app_name: CONFIG.syslog_app_name.to_string(),
        ssh_user:      CONFIG.ssh_user.to_string(),
        ssh_password:  CONFIG.ssh_password.to_string(),
        com1_tx_pin:   CONFIG.com1_tx_pin.to_string(),
        com1_rx_pin:   CONFIG.com1_rx_pin.to_string(),
        com1_baud:     CONFIG.com1_baud.to_string(),
        com2_tx_pin:   CONFIG.com2_tx_pin.to_string(),
        com2_rx_pin:   CONFIG.com2_rx_pin.to_string(),
        com2_baud:     CONFIG.com2_baud.to_string(),
        cdc_enable:    CONFIG.cdc_enable.to_string(),
        cdc_baud:      CONFIG.cdc_baud.to_string(),
        display_enable: CONFIG.display_enable.to_string(),
        display_port:  CONFIG.display_port.to_string(),
    };
    let nvs_config = httpserver::load_config(cfg_defaults.clone());

    // ── Button control thread — always started (GPIO0 / BOOT button) ──────────
    // Short press: cycle display page; Long press (3s): factory reset to cfg.toml defaults
    btn_ctrl::start_button_thread(peripherals.pins.gpio0);

    // ── Display init (only when enabled) ──────────────────────────────────
    if nvs_config.display_enable == "true" {
        let com1_buf = serial_display::SerialRxBuffer::new();
        let com2_buf = serial_display::SerialRxBuffer::new();
        let usb_buf  = serial_display::SerialRxBuffer::new();
        usb_host::set_display_rx_bufs(com1_buf.clone(), com2_buf.clone(), usb_buf.clone());
        serial_display::start_display_thread(
            peripherals.spi3,
            peripherals.pins.gpio48,
            peripherals.pins.gpio45,
            peripherals.pins.gpio40,
            peripherals.pins.gpio39,
            peripherals.pins.gpio38,
            com1_buf,
            com2_buf,
            usb_buf,
        );
        usb_host::set_display_port(&nvs_config.display_port);
        info!("Display enabled, port: {}", nvs_config.display_port);
    } else {
        info!("Display disabled");
    }

    // WiFi connect (using NVS config — falls back to cfg.toml if not set)
    println!("Connecting to WiFi: {}", nvs_config.wifi_ssid);
    let mut wifi_dev = wifi::wifi_connect(peripherals.modem, &nvs_config.wifi_ssid, &nvs_config.wifi_psk);

    // Create shared status and populate IP/SSID after WiFi connects
    let dev_status = httpserver::StatusInfo::new();
    match &wifi_dev {
        Ok(w) => {
            println!("WiFi connected.");
            // Apply static IP if configured
            if nvs_config.ip_mode == "static" {
                match wifi::configure_static_ip(w, &nvs_config.ip_address, &nvs_config.subnet_mask, &nvs_config.gateway, &nvs_config.dns) {
                    Ok(_) => println!("Static IP configured successfully"),
                    Err(e) => println!("Failed to set static IP: {:?}, falling back to DHCP", e),
                }
            }
            if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                println!("IP address: {}", ip_info.ip);
                dev_status.set_wifi(
                    &ip_info.ip.to_string(),
                    &nvs_config.wifi_ssid,
                    wifi::get_rssi(),
                );
            }
        },
        Err(e) => println!("WiFi connect failed: {:?}", e),
    }

    // Start HTTP config server (login protected, NVS-backed settings)
    let gpio_pwm_state = gpio_ctrl::GpioPwmState::new();
    let config_state = httpserver::ConfigState::new(nvs_config.clone(), cfg_defaults, dev_status.clone(), gpio_pwm_state.clone());
    let _http_server = match httpserver::start_http_server(config_state) {
        Ok(s)  => { println!("HTTP config server started on port 80"); Some(s) }
        Err(e) => { println!("HTTP config server failed to start: {:?}", e); None }
    };

    // Start WebSocket sender thread (drains ring buffers → WS clients)
    usb_host::start_ws_sender_thread();

    // Provide info-page data to the display thread (only when display is enabled)
    if nvs_config.display_enable == "true" {
        serial_display::set_display_aux(serial_display::DisplayAux {
            status: dev_status.clone(),
            gpio_state: gpio_pwm_state.clone(),
            com1_baud: nvs_config.com1_baud.clone(),
            com1_tx: nvs_config.com1_tx_pin.clone(),
            com1_rx: nvs_config.com1_rx_pin.clone(),
            com2_baud: nvs_config.com2_baud.clone(),
            com2_tx: nvs_config.com2_tx_pin.clone(),
            com2_rx: nvs_config.com2_rx_pin.clone(),
            cdc_baud: nvs_config.cdc_baud.clone(),
        });
    }

    // Syslog init (after wifi is up)
    if nvs_config.syslog_enable == "true" {
        println!("Initializing syslog to {} ...", nvs_config.syslog_server);
        thread::sleep(Duration::from_secs(5));
        match syslogger::init_logger(&nvs_config.syslog_server, &nvs_config.syslog_enable, &nvs_config.syslog_host_name, &nvs_config.syslog_app_name) {
            Ok(_) => {
                log::set_max_level(log::LevelFilter::Info);
                println!("Syslog logger initialized successfully");
                info!("Syslog logger initialized successfully");
                if let Ok(w) = wifi_dev.as_ref() {
                    if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                        info!("WiFi IP address: {}", ip_info.ip);
                    }
                }
            },
            Err(e) => {
                println!("Failed to initialize syslog: {:?}, falling back to ESP logger", e);
                let _ = esp_idf_svc::log::EspLogger::initialize_default();
                log::set_max_level(log::LevelFilter::Info);
            }
        }
    } else {
        esp_idf_svc::log::EspLogger::initialize_default();
        log::set_max_level(log::LevelFilter::Info);
        info!("Using default ESP console logger (syslog disabled)");
    }

    // NTP Server
    let sntp_conf = SntpConf {
        servers: ["time.aws.com",
                    "time.google.com",
                    "time.cloudflare.com",
                    "ntp.nict.jp"],
        operating_mode: OperatingMode::Poll,
        sync_mode: SyncMode::Immediate,
    };
    let ntp = EspSntp::new(&sntp_conf).unwrap();

    // NTP Sync
    info!("NTP Sync Start..");

    // wait for sync
    let mut sync_count = 0;
    while ntp.get_sync_status() != SyncStatus::Completed {
        sync_count += 1;
        if sync_count > 1000 {
            info!("NTP Sync Timeout");
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let now = SystemTime::now();
    let dt_now : DateTime<Utc> = now.into();
    let formatted = format!("{}", dt_now.format("%Y-%m-%d %H:%M:%S"));
    info!("NTP Sync Completed: {}", formatted);

    // Start USB host + SSH bridge
    // Leak NVS strings to obtain &'static str required by usb_host::start.
    let ssh_user_s: &'static str = Box::leak(nvs_config.ssh_user.clone().into_boxed_str());
    let ssh_pass_s: &'static str = Box::leak(nvs_config.ssh_password.clone().into_boxed_str());
    usb_host::start(
        ssh_user_s,
        ssh_pass_s,
        nvs_config.com1_tx_pin.parse().unwrap_or(17),
        nvs_config.com1_rx_pin.parse().unwrap_or(18),
        nvs_config.com1_baud.parse().unwrap_or(115200),
        nvs_config.com2_tx_pin.parse().unwrap_or(19),
        nvs_config.com2_rx_pin.parse().unwrap_or(20),
        nvs_config.com2_baud.parse().unwrap_or(115200),
        nvs_config.cdc_enable == "true",
        nvs_config.cdc_baud.parse().unwrap_or(115200),
        gpio_pwm_state.clone(),
    );

    println!("Starting PWM init...");
    let timer_config_out_current_0 = TimerConfig::default().frequency(1.kHz().into())
        .resolution(esp_idf_hal::ledc::config::Resolution::Bits14);
    let timer_driver_0 = LedcTimerDriver::new(peripherals.ledc.timer0, &timer_config_out_current_0).unwrap();
    let mut pwm_driver_0 = LedcDriver::new(peripherals.ledc.channel0, &timer_driver_0, peripherals.pins.gpio1).unwrap();
    pwm_driver_0.set_duty(0).expect("Set duty failure");
    let max_duty_0 = pwm_driver_0.get_max_duty();
    info!("Max duty: {}", max_duty_0);

    let timer_config_out_current_1 = TimerConfig::default().frequency(1.kHz().into())
        .resolution(esp_idf_hal::ledc::config::Resolution::Bits14);
    let timer_driver_1 = LedcTimerDriver::new(peripherals.ledc.timer1, &timer_config_out_current_1).unwrap();
    let mut pwm_driver_1 = LedcDriver::new(peripherals.ledc.channel1, &timer_driver_1, peripherals.pins.gpio2).unwrap();
    pwm_driver_1.set_duty(0).expect("Set duty failure");
    let max_duty_1 = pwm_driver_1.get_max_duty();
    info!("Max duty: {}", max_duty_1);

    // ── GPIO 4-9, 12 output init ─────────────────────────────────────────────
    println!("Initializing GPIO 4-9,12 outputs...");
    let mut gpio_out: [PinDriver<AnyOutputPin, Output>; 7] = [
        PinDriver::output(peripherals.pins.gpio4.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio5.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio6.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio7.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio8.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio9.downgrade_output()).unwrap(),
        PinDriver::output(peripherals.pins.gpio12.downgrade_output()).unwrap(),
    ];
    for pin in gpio_out.iter_mut() {
        pin.set_low().unwrap();
    }

    // ── PWM GPIO 10-11 (LEDC timer2/3, channel2/3) ────────────────────────
    println!("Initializing PWM GPIO 10-11...");
    let timer_cfg_10 = TimerConfig::default().frequency(1.kHz().into())
        .resolution(esp_idf_hal::ledc::config::Resolution::Bits14);
    let timer_driver_2 = LedcTimerDriver::new(peripherals.ledc.timer2, &timer_cfg_10).unwrap();
    let mut pwm_gpio10 = LedcDriver::new(peripherals.ledc.channel2, &timer_driver_2, peripherals.pins.gpio10).unwrap();
    pwm_gpio10.set_duty(0).expect("Set duty failure gpio10");
    let max_duty_gpio10 = pwm_gpio10.get_max_duty();

    let timer_cfg_11 = TimerConfig::default().frequency(1.kHz().into())
        .resolution(esp_idf_hal::ledc::config::Resolution::Bits14);
    let timer_driver_3 = LedcTimerDriver::new(peripherals.ledc.timer3, &timer_cfg_11).unwrap();
    let mut pwm_gpio11 = LedcDriver::new(peripherals.ledc.channel3, &timer_driver_3, peripherals.pins.gpio11).unwrap();
    pwm_gpio11.set_duty(0).expect("Set duty failure gpio11");
    let max_duty_gpio11 = pwm_gpio11.get_max_duty();

    info!("PWM gpio10 max_duty: {}, gpio11 max_duty: {}", max_duty_gpio10, max_duty_gpio11);

    // ── ADC2 init ─────────────────────────────────────────────
    let adc2 = AdcDriver::new(peripherals.adc2).unwrap();
    let adc_config = AdcChannelConfig {
        attenuation: DB_11,
        ..Default::default()
    };

    // ── ADC2 Channel 2 (GPIO13) init ─────────────────────────────────────────────
    let mut adc13_pin = AdcChannelDriver::new(&adc2, peripherals.pins.gpio13, &adc_config).unwrap();

    // ADC2 Channel 3 (GPIO14) init ─────────────────────────────────────────────
    let mut adc14_pin = AdcChannelDriver::new(&adc2, peripherals.pins.gpio14, &adc_config).unwrap();

    // ── Internal temperature sensor init ──────────────────────────────────
    let temp_cfg = TempSensorConfig::default();
    let mut temp_sensor = TempSensorDriver::new(&temp_cfg, peripherals.temp_sensor).unwrap();
    temp_sensor.enable().unwrap();

    let mut count = 0u32;
    loop {
        // ── Apply GPIO 4-9 desired state ──────────────────────────────────
        {
            let states = gpio_pwm_state.get_gpio();
            for (i, pin) in gpio_out.iter_mut().enumerate() {
                if states[i] { pin.set_high().ok(); } else { pin.set_low().ok(); }
            }
        }

        // ── Apply PWM GPIO 10-11 desired duty ─────────────────────────────
        {
            let duties = gpio_pwm_state.get_pwm();
            let raw10 = (duties[0] as u32 * max_duty_gpio10) / 100;
            let raw11 = (duties[1] as u32 * max_duty_gpio11) / 100;
            pwm_gpio10.set_duty(raw10).ok();
            pwm_gpio11.set_duty(raw11).ok();
        }

        if count % 50 == 0 {
            let dc_in_voltage : f32 =  adc2.read(&mut adc13_pin).unwrap() as f32/ 80.098; // Already adjusted conversion factor for voltage divider
            let dc_out_voltage : f32 =  adc2.read(&mut adc14_pin).unwrap() as f32 / 80.098; // Already adjusted conversion factor for voltage divider
            dev_status.set_voltages(dc_in_voltage, dc_out_voltage);
            let temp_str = match temp_sensor.get_celsius() {
                Ok(t) => { dev_status.set_chip_temp(t); format!("{:.1} °C", t) },
                Err(_) => "--".to_string(),
            };
            info!("Internal temp: {}, DCIN: {:.2} V, DCOUT: {:.2} V", temp_str, dc_in_voltage, dc_out_voltage);
        }

        // Check connection state via EspWifi before calling get_rssi(),
        // because esp_wifi_sta_get_rssi crashes (LoadProhibited) when
        // WiFi is not connected.
        let is_connected = wifi_dev.as_ref()
            .map(|w| w.is_connected().unwrap_or(false))
            .unwrap_or(false);

        if !is_connected {
            if count % 30 == 0 {
                if let Ok(w) = wifi_dev.as_mut() {
                    wifi_reconnect(w);
                }
            }
        } else {
            let rssi = wifi::get_rssi();
            dev_status.set_rssi(rssi);
            if count % 30 == 0 {
                if let Ok(w) = wifi_dev.as_ref() {
                    if let Ok(ip_info) = w.sta_netif().get_ip_info() {
                        dev_status.set_wifi(&ip_info.ip.to_string(), &nvs_config.wifi_ssid, rssi);
                    }
                }
            }
        }
        count = count.wrapping_add(1);
        thread::sleep(Duration::from_millis(100));
    }
}

fn wifi_reconnect(wifi_dev: &mut EspWifi) -> bool{
    unsafe {
        esp_idf_sys::esp_wifi_start();
    }
    match wifi_dev.connect() {
        Ok(_) => { info!("Wifi connecting requested."); true},
        Err(ref e) => { info!("{:?}", e); false }
    }
}