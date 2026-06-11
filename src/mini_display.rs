use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use log::info;
use esp_idf_hal::i2c::*;
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, ascii::FONT_5X8, MonoTextStyle},
    pixelcolor::BinaryColor,
    text::Text,
    geometry::Point,
    prelude::*,
    primitives::{Line, PrimitiveStyle},
};

/// Display information structure
#[derive(Clone)]
pub struct DisplayInfo {
    pub wifi_connected: bool,
    pub ip_address: String,
    pub rssi: i8,
    pub dc_in_voltage: f32,
    pub com1_baud: u32,
}

// Custom PartialEq to handle floating point comparison
impl PartialEq for DisplayInfo {
    fn eq(&self, other: &Self) -> bool {
        self.wifi_connected == other.wifi_connected
            && self.ip_address == other.ip_address
            && self.rssi == other.rssi
            && (self.dc_in_voltage - other.dc_in_voltage).abs() < 0.01
            && self.com1_baud == other.com1_baud
    }
}

impl Default for DisplayInfo {
    fn default() -> Self {
        Self {
            wifi_connected: false,
            ip_address: String::from("---"),
            rssi: 0,
            dc_in_voltage: 0.0,
            com1_baud: 0,
        }
    }
}

/// Shared state for display information
static DISPLAY_INFO: Mutex<DisplayInfo> = Mutex::new(DisplayInfo {
    wifi_connected: false,
    ip_address: String::new(),
    rssi: 0,
    dc_in_voltage: 0.0,
    com1_baud: 0,
});

/// Overlay message (title, body) shown on top of normal content.
static MINI_MESSAGE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Show an overlay message on the mini display.
pub fn show_message(title: &str, text: &str) {
    *MINI_MESSAGE.lock().unwrap() = Some((title.to_string(), text.to_string()));
}

/// Clear the overlay message and return to normal display.
pub fn clear_message() {
    *MINI_MESSAGE.lock().unwrap() = None;
}

/// Update all display information
pub fn update_display_info(wifi_connected: bool, ip: &str, rssi: i8, dc_in_voltage: f32, com1_baud: u32) {
    let mut info = DISPLAY_INFO.lock().unwrap();
    info.wifi_connected = wifi_connected;
    info.ip_address = ip.to_string();
    info.rssi = rssi;
    info.dc_in_voltage = dc_in_voltage;
    info.com1_baud = com1_baud;
}

/// Start the mini display thread (SSD1306 via I2C)
/// GPIO3 = SCL, GPIO46 = SDA
pub fn start_mini_display_thread(
    i2c: I2cDriver<'static>,
) {
    thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            info!("Mini display thread started (SSD1306)");
            
            // Create display interface
            let interface = I2CDisplayInterface::new(i2c);
            
            // Create display instance (128x64, 0x3C address)
            let mut display = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
                .into_buffered_graphics_mode();
            
            // Initialize display
            if let Err(e) = display.init() {
                info!("Failed to initialize SSD1306: {:?}", e);
                return;
            }
            
            // Set brightness to maximum using Brightness from prelude
            display.set_brightness(Brightness::BRIGHTEST).ok();
            
            info!("SSD1306 initialized successfully");
            
            let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
            let small_style = MonoTextStyle::new(&FONT_5X8, BinaryColor::On);
            let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
            
            let mut last_info: Option<DisplayInfo> = None;
            let mut last_message: Option<(String, String)> = None;
            
            loop {
                // Get current display info and overlay message
                let info = DISPLAY_INFO.lock().unwrap().clone();
                let msg  = MINI_MESSAGE.lock().unwrap().clone();
                
                // Redraw if info or overlay message changed
                if last_info.as_ref() != Some(&info) || last_message != msg {
                    // Clear display
                    display.clear(BinaryColor::Off).ok();
                    
                    if let Some((ref title, ref body)) = msg {
                        // ── Overlay mode: show title + body ──────────────
                        Text::new(title, Point::new(2, 10), text_style)
                            .draw(&mut display)
                            .ok();
                        Line::new(Point::new(0, 14), Point::new(127, 14))
                            .into_styled(line_style)
                            .draw(&mut display)
                            .ok();
                        let mut y: i32 = 28;
                        for line in body.split('\n') {
                            Text::new(line, Point::new(2, y), small_style)
                                .draw(&mut display)
                                .ok();
                            y += 12;
                        }
                    } else {
                        // ── Normal mode ──────────────────────────────────
                        // Draw header line
                        Line::new(Point::new(0, 12), Point::new(127, 12))
                            .into_styled(line_style)
                            .draw(&mut display)
                            .ok();
                        
                        // Line 1: WiFi Status with RSSI (top)
                        let wifi_status = if info.wifi_connected {
                            format!("WiFi:{}dBm", info.rssi)
                        } else {
                            String::from("WiFi: --")
                        };
                        Text::new(&wifi_status, Point::new(2, 8), small_style)
                            .draw(&mut display)
                            .ok();
                        
                        // Line 2: IP Address
                        let ip_text = format!("IP:{}", info.ip_address);
                        Text::new(&ip_text, Point::new(2, 24), text_style)
                            .draw(&mut display)
                            .ok();
                        
                        // Line 3: DC IN Voltage
                        let dc_text = format!("DC_IN:{:.2}V", info.dc_in_voltage);
                        Text::new(&dc_text, Point::new(2, 40), text_style)
                            .draw(&mut display)
                            .ok();
                        
                        // Line 4: COM1 Baud Rate
                        let baud_text = if info.com1_baud > 0 {
                            format!("COM1:{}bps", info.com1_baud)
                        } else {
                            String::from("COM1:---")
                        };
                        Text::new(&baud_text, Point::new(2, 56), text_style)
                            .draw(&mut display)
                            .ok();
                    }
                    
                    // Flush to display
                    if let Err(e) = display.flush() {
                        info!("Failed to flush SSD1306: {:?}", e);
                    }
                    
                    last_info = Some(info);
                    last_message = msg;
                }
                
                // Update every 250 ms for responsive overlay updates
                thread::sleep(Duration::from_millis(250));
            }
        })
        .expect("Failed to spawn mini display thread");
}
