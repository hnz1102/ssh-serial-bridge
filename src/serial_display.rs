use std::sync::{Arc, Mutex};
use std::thread;
use core::sync::atomic::{AtomicU8, Ordering};
use log::info;
use esp_idf_hal::{gpio::*, prelude::*, spi, delay::FreeRtos};
use esp_idf_hal::spi::config::MODE_0;
use esp_idf_hal::spi::Dma;
use display_interface_spi::SPIInterface;
use mipidsi::{Builder, models::ST7789, options::{Orientation, Rotation}};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X12, ascii::FONT_10X20, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::Rgb565,
    text::Text,
    geometry::{Point, Size},
    primitives::{Rectangle, PrimitiveStyle},
    prelude::*,
};

/// Terminal grid dimensions (320x240 display, 6x12 font).
/// Reserve top 20px for header → 220px / 12 = 18 rows.
const TERM_COLS: usize = 53;
const TERM_ROWS: usize = 18;
const CHAR_W: i32 = 6;
const CHAR_H: i32 = 12;
/// Header height in pixels.
const HEADER_H: i32 = 20;

// ─── Display page management ─────────────────────────────────────────────
pub const PAGE_INFO: u8 = 0;
pub const PAGE_COM1: u8 = 1;
pub const PAGE_COM2: u8 = 2;
pub const PAGE_USB:  u8 = 3;
const PAGE_COUNT: u8 = 4;

static CURRENT_PAGE: AtomicU8 = AtomicU8::new(PAGE_COM1);

pub fn next_page() {
    let cur = CURRENT_PAGE.load(Ordering::Relaxed);
    CURRENT_PAGE.store((cur + 1) % PAGE_COUNT, Ordering::SeqCst);
}

pub fn set_page(page: u8) {
    if page < PAGE_COUNT {
        CURRENT_PAGE.store(page, Ordering::SeqCst);
    }
}

pub fn current_page() -> u8 {
    CURRENT_PAGE.load(Ordering::Relaxed)
}

// ─── Shared info for Info page (set from main after init) ────────────────
pub struct DisplayAux {
    pub status: crate::httpserver::StatusInfo,
    pub gpio_state: crate::gpio_ctrl::GpioPwmState,
    pub com1_baud: String,
    pub com1_tx: String,
    pub com1_rx: String,
    pub com2_baud: String,
    pub com2_tx: String,
    pub com2_rx: String,
    pub cdc_baud: String,
}

static DISPLAY_AUX: Mutex<Option<DisplayAux>> = Mutex::new(None);

pub fn set_display_aux(aux: DisplayAux) {
    *DISPLAY_AUX.lock().unwrap() = Some(aux);
}

// ─── ANSI color palette (standard 8 + bright 8) ──────────────────────────
const ANSI_COLORS: [Rgb565; 16] = [
    Rgb565::new(0, 0, 0),          // 0 black
    Rgb565::new(21, 0, 0),         // 1 red       (170,0,0)
    Rgb565::new(0, 42, 0),         // 2 green     (0,170,0)
    Rgb565::new(21, 42, 0),        // 3 yellow    (170,170,0)
    Rgb565::new(0, 0, 21),         // 4 blue      (0,0,170)
    Rgb565::new(21, 0, 21),        // 5 magenta   (170,0,170)
    Rgb565::new(0, 42, 21),        // 6 cyan      (0,170,170)
    Rgb565::new(21, 42, 21),       // 7 white     (170,170,170)
    Rgb565::new(10, 21, 10),       // 8 bright black (85,85,85)
    Rgb565::new(31, 0, 0),         // 9 bright red
    Rgb565::new(0, 63, 0),         // 10 bright green
    Rgb565::new(31, 63, 0),        // 11 bright yellow
    Rgb565::new(0, 0, 31),         // 12 bright blue
    Rgb565::new(31, 0, 31),        // 13 bright magenta
    Rgb565::new(0, 63, 31),        // 14 bright cyan
    Rgb565::new(31, 63, 31),       // 15 bright white
];

const DEFAULT_FG: Rgb565 = ANSI_COLORS[7];  // white
const DEFAULT_BG: Rgb565 = ANSI_COLORS[0];  // black

// ─── Terminal cell ────────────────────────────────────────────────────────
#[derive(Copy, Clone)]
struct Cell {
    ch: char,
    fg: Rgb565,
    bg: Rgb565,
}

impl Cell {
    fn blank() -> Self {
        Self { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG }
    }
}

// ─── ANSI parser state machine ───────────────────────────────────────────
#[derive(PartialEq)]
enum ParseState {
    Normal,
    Esc,        // got ESC
    Csi,        // got ESC[
    OscOrOther, // got ESC] or other; eat until ST
}

struct AnsiTerminal {
    cells: [[Cell; TERM_COLS]; TERM_ROWS],
    cursor_row: usize,
    cursor_col: usize,
    saved_row: usize,
    saved_col: usize,
    fg: Rgb565,
    bg: Rgb565,
    bold: bool,
    parse_state: ParseState,
    csi_params: Vec<u8>,
    dirty: bool,
}

impl AnsiTerminal {
    fn new() -> Self {
        Self {
            cells: [[Cell::blank(); TERM_COLS]; TERM_ROWS],
            cursor_row: 0,
            cursor_col: 0,
            saved_row: 0,
            saved_col: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            parse_state: ParseState::Normal,
            csi_params: Vec::new(),
            dirty: true,
        }
    }

    fn clear_all(&mut self) {
        for r in 0..TERM_ROWS {
            for c in 0..TERM_COLS {
                self.cells[r][c] = Cell::blank();
            }
        }
        self.dirty = true;
    }

    /// Scroll the terminal up by one line.
    fn scroll_up(&mut self) {
        for r in 1..TERM_ROWS {
            self.cells[r - 1] = self.cells[r];
        }
        self.cells[TERM_ROWS - 1] = [Cell::blank(); TERM_COLS];
        self.dirty = true;
    }

    /// Put a character at cursor position and advance.
    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= TERM_COLS {
            // auto-wrap
            self.cursor_col = 0;
            self.cursor_row += 1;
        }
        if self.cursor_row >= TERM_ROWS {
            self.scroll_up();
            self.cursor_row = TERM_ROWS - 1;
        }
        self.cells[self.cursor_row][self.cursor_col] = Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
        };
        self.cursor_col += 1;
        self.dirty = true;
    }

    /// Feed raw bytes from serial into the ANSI parser.
    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match self.parse_state {
                ParseState::Normal => {
                    match b {
                        0x1B => self.parse_state = ParseState::Esc,
                        b'\n' => {
                            self.cursor_row += 1;
                            if self.cursor_row >= TERM_ROWS {
                                self.scroll_up();
                                self.cursor_row = TERM_ROWS - 1;
                            }
                            self.dirty = true;
                        }
                        b'\r' => {
                            self.cursor_col = 0;
                            self.dirty = true;
                        }
                        b'\x08' => {
                            // backspace
                            if self.cursor_col > 0 {
                                self.cursor_col -= 1;
                            }
                            self.dirty = true;
                        }
                        b'\t' => {
                            // tab: advance to next 8-col stop
                            let next = (self.cursor_col + 8) & !7;
                            let next = if next > TERM_COLS { TERM_COLS } else { next };
                            while self.cursor_col < next {
                                self.put_char(' ');
                            }
                        }
                        0x00..=0x1F => {} // ignore other control chars
                        _ => self.put_char(b as char),
                    }
                }
                ParseState::Esc => {
                    match b {
                        b'[' => {
                            self.csi_params.clear();
                            self.parse_state = ParseState::Csi;
                        }
                        b']' | b'(' | b')' | b'#' => {
                            self.parse_state = ParseState::OscOrOther;
                        }
                        b'7' => {
                            // save cursor
                            self.saved_row = self.cursor_row;
                            self.saved_col = self.cursor_col;
                            self.parse_state = ParseState::Normal;
                        }
                        b'8' => {
                            // restore cursor
                            self.cursor_row = self.saved_row;
                            self.cursor_col = self.saved_col;
                            self.parse_state = ParseState::Normal;
                        }
                        b'c' => {
                            // full reset
                            self.clear_all();
                            self.cursor_row = 0;
                            self.cursor_col = 0;
                            self.fg = DEFAULT_FG;
                            self.bg = DEFAULT_BG;
                            self.bold = false;
                            self.parse_state = ParseState::Normal;
                        }
                        b'D' => {
                            // index (scroll up)
                            self.cursor_row += 1;
                            if self.cursor_row >= TERM_ROWS {
                                self.scroll_up();
                                self.cursor_row = TERM_ROWS - 1;
                            }
                            self.parse_state = ParseState::Normal;
                        }
                        b'M' => {
                            // reverse index (scroll down)
                            if self.cursor_row == 0 {
                                // scroll down: move rows 0..N-2 to 1..N-1
                                for r in (1..TERM_ROWS).rev() {
                                    self.cells[r] = self.cells[r - 1];
                                }
                                self.cells[0] = [Cell::blank(); TERM_COLS];
                            } else {
                                self.cursor_row -= 1;
                            }
                            self.dirty = true;
                            self.parse_state = ParseState::Normal;
                        }
                        _ => self.parse_state = ParseState::Normal,
                    }
                }
                ParseState::Csi => {
                    if b >= b'0' && b <= b'9' || b == b';' || b == b'?' {
                        self.csi_params.push(b);
                    } else {
                        // b is the final byte (command)
                        self.handle_csi(b);
                        self.parse_state = ParseState::Normal;
                    }
                }
                ParseState::OscOrOther => {
                    // eat until BEL or ST (ESC\)
                    if b == 0x07 || b == b'\\' || (b >= 0x40 && b <= 0x7E && b != b'[') {
                        self.parse_state = ParseState::Normal;
                    }
                }
            }
        }
    }

    /// Parse CSI parameter string into a list of u16 values.
    fn parse_params(&self) -> Vec<u16> {
        if self.csi_params.is_empty() {
            return Vec::new();
        }
        // Filter out '?' prefix
        let s: Vec<u8> = self.csi_params.iter()
            .filter(|&&b| b != b'?')
            .cloned()
            .collect();
        let s = std::str::from_utf8(&s).unwrap_or("");
        s.split(';')
            .map(|p| p.parse::<u16>().unwrap_or(0))
            .collect()
    }

    fn handle_csi(&mut self, cmd: u8) {
        let params = self.parse_params();
        let p1 = *params.first().unwrap_or(&0);
        let p2 = *params.get(1).unwrap_or(&0);

        match cmd {
            b'A' => {
                // CUU – Cursor Up
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.dirty = true;
            }
            b'B' => {
                // CUD – Cursor Down
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = (self.cursor_row + n).min(TERM_ROWS - 1);
                self.dirty = true;
            }
            b'C' => {
                // CUF – Cursor Forward
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = (self.cursor_col + n).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'D' => {
                // CUB – Cursor Backward
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = self.cursor_col.saturating_sub(n);
                self.dirty = true;
            }
            b'H' | b'f' => {
                // CUP – Cursor Position  ESC[row;colH
                let row = if p1 == 0 { 1 } else { p1 as usize };
                let col = if p2 == 0 { 1 } else { p2 as usize };
                self.cursor_row = (row - 1).min(TERM_ROWS - 1);
                self.cursor_col = (col - 1).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'J' => {
                // ED – Erase in Display
                match p1 {
                    0 => {
                        // erase from cursor to end
                        for c in self.cursor_col..TERM_COLS {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                        for r in (self.cursor_row + 1)..TERM_ROWS {
                            self.cells[r] = [Cell::blank(); TERM_COLS];
                        }
                    }
                    1 => {
                        // erase from start to cursor
                        for r in 0..self.cursor_row {
                            self.cells[r] = [Cell::blank(); TERM_COLS];
                        }
                        for c in 0..=self.cursor_col.min(TERM_COLS - 1) {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    2 | 3 => {
                        // erase entire screen
                        self.clear_all();
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            b'K' => {
                // EL – Erase in Line
                match p1 {
                    0 => {
                        for c in self.cursor_col..TERM_COLS {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    1 => {
                        for c in 0..=self.cursor_col.min(TERM_COLS - 1) {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    2 => {
                        self.cells[self.cursor_row] = [Cell::blank(); TERM_COLS];
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            b'G' => {
                // CHA – Cursor Horizontal Absolute
                let col = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = (col - 1).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'd' => {
                // VPA – Vertical Position Absolute
                let row = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = (row - 1).min(TERM_ROWS - 1);
                self.dirty = true;
            }
            b'S' => {
                // SU – Scroll Up
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for _ in 0..n { self.scroll_up(); }
            }
            b'T' => {
                // SD – Scroll Down
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for _ in 0..n {
                    for r in (1..TERM_ROWS).rev() {
                        self.cells[r] = self.cells[r - 1];
                    }
                    self.cells[0] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'L' => {
                // IL – Insert Lines
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_ROWS - self.cursor_row);
                for _ in 0..n {
                    for r in (self.cursor_row + 1..TERM_ROWS).rev() {
                        self.cells[r] = self.cells[r - 1];
                    }
                    self.cells[self.cursor_row] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'M' => {
                // DL – Delete Lines
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_ROWS - self.cursor_row);
                for _ in 0..n {
                    for r in self.cursor_row..(TERM_ROWS - 1) {
                        self.cells[r] = self.cells[r + 1];
                    }
                    self.cells[TERM_ROWS - 1] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'P' => {
                // DCH – Delete Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_COLS - self.cursor_col);
                for c in self.cursor_col..(TERM_COLS - n) {
                    self.cells[self.cursor_row][c] = self.cells[self.cursor_row][c + n];
                }
                for c in (TERM_COLS - n)..TERM_COLS {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'@' => {
                // ICH – Insert Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_COLS - self.cursor_col);
                for c in (self.cursor_col + n..TERM_COLS).rev() {
                    self.cells[self.cursor_row][c] = self.cells[self.cursor_row][c - n];
                }
                for c in self.cursor_col..(self.cursor_col + n) {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'X' => {
                // ECH – Erase Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for c in self.cursor_col..(self.cursor_col + n).min(TERM_COLS) {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'm' => {
                // SGR – Select Graphic Rendition
                if params.is_empty() {
                    self.sgr_reset();
                } else {
                    let mut i = 0;
                    while i < params.len() {
                        match params[i] {
                            0 => self.sgr_reset(),
                            1 => self.bold = true,
                            22 => self.bold = false,
                            7 => {
                                // reverse video
                                std::mem::swap(&mut self.fg, &mut self.bg);
                            }
                            27 => {
                                // un-reverse
                                std::mem::swap(&mut self.fg, &mut self.bg);
                            }
                            // Foreground standard
                            30..=37 => {
                                let idx = (params[i] - 30) as usize;
                                self.fg = if self.bold { ANSI_COLORS[idx + 8] } else { ANSI_COLORS[idx] };
                            }
                            39 => self.fg = DEFAULT_FG,
                            // Background standard
                            40..=47 => {
                                let idx = (params[i] - 40) as usize;
                                self.bg = ANSI_COLORS[idx];
                            }
                            49 => self.bg = DEFAULT_BG,
                            // Foreground bright
                            90..=97 => {
                                let idx = (params[i] - 90 + 8) as usize;
                                self.fg = ANSI_COLORS[idx];
                            }
                            // Background bright
                            100..=107 => {
                                let idx = (params[i] - 100 + 8) as usize;
                                self.bg = ANSI_COLORS[idx];
                            }
                            // 256-color mode: 38;5;n or 48;5;n
                            38 => {
                                if i + 2 < params.len() && params[i + 1] == 5 {
                                    self.fg = color256_to_rgb565(params[i + 2]);
                                    i += 2;
                                }
                            }
                            48 => {
                                if i + 2 < params.len() && params[i + 1] == 5 {
                                    self.bg = color256_to_rgb565(params[i + 2]);
                                    i += 2;
                                }
                            }
                            _ => {} // ignore unsupported
                        }
                        i += 1;
                    }
                }
                // SGR doesn't mark positional dirty, but color changes
                // will appear on next characters written.
            }
            b's' => {
                // SCP – Save Cursor Position
                self.saved_row = self.cursor_row;
                self.saved_col = self.cursor_col;
            }
            b'u' => {
                // RCP – Restore Cursor Position
                self.cursor_row = self.saved_row;
                self.cursor_col = self.saved_col;
                self.dirty = true;
            }
            b'h' | b'l' | b'r' | b'n' | b'c' => {
                // mode set/reset, scroll region, device status — ignore
            }
            _ => {} // unknown CSI
        }
    }

    fn sgr_reset(&mut self) {
        self.fg = DEFAULT_FG;
        self.bg = DEFAULT_BG;
        self.bold = false;
    }
}

/// Map 256-color index to Rgb565.
fn color256_to_rgb565(n: u16) -> Rgb565 {
    if n < 16 {
        return ANSI_COLORS[n as usize];
    }
    if n < 232 {
        // 6x6x6 color cube: 16 + 36*r + 6*g + b  (r,g,b ∈ 0..5)
        let idx = (n - 16) as u32;
        let b_val = (idx % 6) as u8;
        let g_val = ((idx / 6) % 6) as u8;
        let r_val = ((idx / 36) % 6) as u8;
        // Scale 0..5 to 5-bit (0..31) for R/B and 6-bit (0..63) for G
        let r5 = (r_val as u16 * 31 + 2) / 5;
        let g6 = (g_val as u16 * 63 + 2) / 5;
        let b5 = (b_val as u16 * 31 + 2) / 5;
        Rgb565::new(r5 as u8, g6 as u8, b5 as u8)
    } else {
        // grayscale ramp: 232..255 → shades 8,18,28,...238
        let shade = ((n - 232) * 10 + 8) as u16;
        let r5 = (shade * 31 + 127) / 255;
        let g6 = (shade * 63 + 127) / 255;
        let b5 = (shade * 31 + 127) / 255;
        Rgb565::new(r5 as u8, g6 as u8, b5 as u8)
    }
}

// ─── Public shared buffer (thin wrapper around AnsiTerminal) ─────────────
#[derive(Clone)]
pub struct SerialRxBuffer {
    inner: Arc<Mutex<AnsiTerminal>>,
}

impl SerialRxBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AnsiTerminal::new())),
        }
    }

    /// Push raw bytes from serial into the ANSI terminal emulator.
    pub fn push_data(&self, data: &[u8]) {
        let mut term = self.inner.lock().unwrap();
        term.feed(data);
    }
}

/// Start the display thread. Call once from main after peripherals init.
pub fn start_display_thread(
    spi_periph: spi::SPI3,
    sclk: Gpio48,
    sdo: Gpio45,
    cs: Gpio40,
    dc_pin: Gpio39,
    rst_pin: Gpio38,
    com1_buf: SerialRxBuffer,
    com2_buf: SerialRxBuffer,
    usb_buf: SerialRxBuffer,
) {
    thread::Builder::new()
        .name("display".into())
        .stack_size(24576)
        .spawn(move || display_task(spi_periph, sclk, sdo, cs, dc_pin, rst_pin,
                                     com1_buf, com2_buf, usb_buf))
        .expect("spawn display thread");
}

fn display_task(
    spi_periph: spi::SPI3,
    sclk: Gpio48,
    sdo: Gpio45,
    cs: Gpio40,
    dc_pin: Gpio39,
    rst_pin: Gpio38,
    com1_buf: SerialRxBuffer,
    com2_buf: SerialRxBuffer,
    usb_buf: SerialRxBuffer,
) {
    let dc = PinDriver::output(dc_pin).unwrap();
    let rst = PinDriver::output(rst_pin).unwrap();

    let spi_config = spi::SpiConfig::new().baudrate(40.MHz().into()).data_mode(MODE_0);
    let spi_driver_config = spi::SpiDriverConfig {
        dma: Dma::Disabled,
        ..Default::default()
    };
    let sdi_not_used: Option<Gpio2> = None;

    let spi_driver = spi::SpiDriver::new(
        spi_periph, sclk, sdo, sdi_not_used, &spi_driver_config,
    ).unwrap();
    let spi_device = spi::SpiDeviceDriver::new(spi_driver, Some(cs), &spi_config).unwrap();

    let mut delay = FreeRtos;
    let di = SPIInterface::new(spi_device, dc);
    let orientation = Orientation::new().rotate(Rotation::Deg90);
    let mut display = Builder::new(ST7789, di)
        .reset_pin(rst)
        .display_size(240, 320)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .color_order(mipidsi::options::ColorOrder::Rgb)
        .orientation(orientation)
        .init(&mut delay)
        .unwrap();

    info!("Display initialized (320x240 multi-page)");

    let header_style = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_GREEN);

    // Initial clear
    display.clear(Rgb565::BLACK).unwrap();

    // Single prev-cell snapshot for the currently viewed terminal page (heap)
    let mut prev_cells = Box::new([[Cell::blank(); TERM_COLS]; TERM_ROWS]);
    let mut force_full_redraw = true;
    let mut last_page = current_page();
    // For info page: track previous content to avoid flicker
    let mut prev_info_lines: Vec<String> = Vec::new();

    // Draw initial header
    draw_header(&mut display, last_page, &header_style);

    loop {
        // ── Page change detection ─────────────────────────────────────
        let page = current_page();
        if page != last_page {
            last_page = page;
            force_full_redraw = true;
            // Reset diff snapshot when switching pages
            *prev_cells = [[Cell::blank(); TERM_COLS]; TERM_ROWS];
        }

        if force_full_redraw {
            display.clear(Rgb565::BLACK).unwrap();
            draw_header(&mut display, page, &header_style);
            prev_info_lines.clear();
        }

        match page {
            PAGE_INFO => {
                draw_info_page(&mut display, force_full_redraw, &mut prev_info_lines);
            }
            PAGE_COM1 => {
                draw_terminal_page(&mut display, &com1_buf, &mut prev_cells, force_full_redraw);
            }
            PAGE_COM2 => {
                draw_terminal_page(&mut display, &com2_buf, &mut prev_cells, force_full_redraw);
            }
            PAGE_USB => {
                draw_terminal_page(&mut display, &usb_buf, &mut prev_cells, force_full_redraw);
            }
            _ => {}
        }

        force_full_redraw = false;
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ─── Header drawing ──────────────────────────────────────────────────────

fn draw_header<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    page: u8,
    style: &MonoTextStyle<Rgb565>,
) {
    let page_num = page + 1;
    let title = match page {
        PAGE_INFO => format!("SYSTEM INFO   {}/{}", page_num, PAGE_COUNT),
        PAGE_COM1 => format!("SERIAL [COM1] {}/{}", page_num, PAGE_COUNT),
        PAGE_COM2 => format!("SERIAL [COM2] {}/{}", page_num, PAGE_COUNT),
        PAGE_USB  => format!("SERIAL [USB]  {}/{}", page_num, PAGE_COUNT),
        _         => format!("UNKNOWN       {}/{}", page_num, PAGE_COUNT),
    };
    let _ = Rectangle::new(Point::new(0, 0), Size::new(320, HEADER_H as u32))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display);
    let _ = Text::new(&title, Point::new(4, 16), *style).draw(display);
    let _ = Rectangle::new(Point::new(0, HEADER_H), Size::new(320, 1))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_GREEN))
        .draw(display);
}

// ─── Info page ───────────────────────────────────────────────────────────

fn draw_info_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
) {
    // Color styles
    let section_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::CSS_LIGHT_GREEN)
        .build();
    let label_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(0, 50, 31))  // cyan
        .build();
    let val_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::WHITE)
        .build();
    let val_yellow = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::YELLOW)
        .build();
    let val_on = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::GREEN)
        .build();
    let val_off = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(21, 0, 0))  // dim red
        .build();
    let val_disabled = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(10, 21, 10))  // gray
        .build();

    // Collect data
    let (ip, ssid, rssi, dc_in, dc_out, chip_temp) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            let ip = aux.status.ip_address.lock().unwrap().clone();
            let ssid = aux.status.ssid.lock().unwrap().clone();
            let rssi = *aux.status.rssi.lock().unwrap();
            let dc_in = *aux.status.dc_in_voltage.lock().unwrap();
            let dc_out = *aux.status.dc_out_voltage.lock().unwrap();
            let chip_temp = *aux.status.chip_temp.lock().unwrap();
            (ip, ssid, rssi, dc_in, dc_out, chip_temp)
        } else {
            ("--".into(), "--".into(), 0i32, 0.0f32, 0.0f32, 0.0f32)
        }
    };

    let active = crate::usb_host::active_device_name();
    let (cdc_conn, vid, pid) = crate::usb_host::cdc_device_info();
    let chip = crate::usb_host::cdc_device_chip_name();
    let cdc_en = crate::usb_host::cdc_is_enabled();
    let cdc_ports = crate::usb_host::cdc_port_count();

    let (c1b, c1t, c1r, c2b, c2t, c2r, cb) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            (aux.com1_baud.clone(), aux.com1_tx.clone(), aux.com1_rx.clone(),
             aux.com2_baud.clone(), aux.com2_tx.clone(), aux.com2_rx.clone(),
             aux.cdc_baud.clone())
        } else {
            ("--".into(), "--".into(), "--".into(),
             "--".into(), "--".into(), "--".into(), "--".into())
        }
    };

    let (gpio_states, pwm_duties) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            (aux.gpio_state.get_gpio(), aux.gpio_state.get_pwm())
        } else {
            ([false; 7], [0u8; 2])
        }
    };

    // Build colored segments per line: Vec<Vec<(&str or String, style)>>
    // We use a simple serialization: join all text into one string per line
    // for change-detection, then draw each segment separately.

    // Each line: Vec<(text, style_index)>
    // style_index: 0=section, 1=label, 2=val, 3=yellow, 4=on, 5=off, 6=disabled
    type Seg = (String, u8);
    let mut rows: Vec<Vec<Seg>> = Vec::new();

    // ── WiFi / Network ───────────────────
    rows.push(vec![("-- Network --".into(), 0)]);
    rows.push(vec![("IP: ".into(), 1), (ip.clone(), 3)]);
    rows.push(vec![
        ("WiFi: ".into(), 1), (ssid.clone(), 2),
        ("  RSSI: ".into(), 1), (format!("{}dBm", rssi), if rssi > -60 { 4 } else if rssi > -80 { 3 } else { 5 }),
    ]);
    rows.push(vec![
        ("DC In: ".into(), 1), (format!("{:.2}V", dc_in), 3),
        ("  DC Out: ".into(), 1), (format!("{:.2}V", dc_out), 3),
    ]);
    rows.push(vec![
        ("ChipTemp: ".into(), 1),
        (format!("{:.1}C", chip_temp),
         if chip_temp < 70.0 { 4 } else if chip_temp < 85.0 { 3 } else { 5 }),
    ]);

    // ── Device ───────────────────────────
    rows.push(vec![("-- Device --".into(), 0)]);
    rows.push(vec![("Active: ".into(), 1), (active.to_string(), 3)]);
    if cdc_en {
        if cdc_conn {
            rows.push(vec![
                ("CDC: ".into(), 1), (chip.to_string(), 3),
                (format!(" {:04X}:{:04X}", vid, pid), 2),
                (format!(" Ports:{}", cdc_ports), 2),
            ]);
        } else {
            rows.push(vec![("CDC: ".into(), 1), ("No device".into(), 6)]);
        }
    } else {
        rows.push(vec![("CDC: ".into(), 1), ("Disabled".into(), 6)]);
    }

    // ── Serial Ports ─────────────────────
    rows.push(vec![("-- Serial --".into(), 0)]);
    rows.push(vec![
        ("COM1 ".into(), 1), (format!("{}bps", c1b), 2),
        (" TX:".into(), 1), (c1t.clone(), 3),
        (" RX:".into(), 1), (c1r.clone(), 3),
    ]);
    rows.push(vec![
        ("COM2 ".into(), 1), (format!("{}bps", c2b), 2),
        (" TX:".into(), 1), (c2t.clone(), 3),
        (" RX:".into(), 1), (c2r.clone(), 3),
    ]);
    rows.push(vec![
        ("USB  ".into(), 1), (format!("{}bps", cb), 2),
    ]);

    // ── GPIO ─────────────────────────────
    rows.push(vec![("-- GPIO --".into(), 0)]);
    let mut gpio_row: Vec<Seg> = Vec::new();
    for i in 0..6 {
        let pin = i + 4;
        gpio_row.push((format!("{}:", pin), 1));
        if gpio_states[i] {
            gpio_row.push((" ON ".into(), 8));
        } else {
            gpio_row.push(("OFF ".into(), 9));
        }
    }
    rows.push(gpio_row);
    // PWM lines use style_index 7 as a marker for bar rendering
    rows.push(vec![
        ("PWM10:".into(), 1), (format!("{:>3}% ", pwm_duties[0]), 3),
        (format!("B{}", pwm_duties[0]), 7),
    ]);
    rows.push(vec![
        ("PWM11:".into(), 1), (format!("{:>3}% ", pwm_duties[1]), 3),
        (format!("B{}", pwm_duties[1]), 7),
    ]);

    // Build flat text per line for change detection
    let text_strs: Vec<String> = rows.iter().map(|segs| {
        segs.iter().map(|(t, _)| t.as_str()).collect::<String>()
    }).collect();

    // Draw changed lines
    for (i, segs) in rows.iter().enumerate() {
        if i >= TERM_ROWS { break; }
        let changed = force || i >= prev_lines.len() || prev_lines[i] != text_strs[i];
        if !changed { continue; }

        let y = HEADER_H + 1 + (i as i32) * CHAR_H;

        // Clear line
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(display);

        // Section header: draw a dim separator line
        if segs.len() == 1 && segs[0].1 == 0 {
            let _ = Rectangle::new(Point::new(0, y), Size::new(320, 1))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 15, 0)))
                .draw(display);
        }

        // Draw each segment
        let mut x = 2;
        for (text, sty) in segs {
            if *sty == 7 {
                // Progress bar: text is "B<duty>" where duty is 0-100
                let duty: u32 = text[1..].parse().unwrap_or(0);
                let bar_max_w: u32 = 200; // max bar width in pixels
                let bar_h: u32 = 8;
                let bar_y = y + 2; // vertically center in 12px line
                // Background track (dark gray)
                let _ = Rectangle::new(
                    Point::new(x, bar_y),
                    Size::new(bar_max_w, bar_h),
                )
                .into_styled(PrimitiveStyle::with_fill(Rgb565::new(4, 8, 4)))
                .draw(display);
                // Filled portion
                let fill_w = (duty * bar_max_w) / 100;
                if fill_w > 0 {
                    let bar_color = if duty < 30 {
                        Rgb565::GREEN
                    } else if duty < 70 {
                        Rgb565::YELLOW
                    } else {
                        Rgb565::new(31, 10, 0) // orange-red
                    };
                    let _ = Rectangle::new(
                        Point::new(x, bar_y),
                        Size::new(fill_w, bar_h),
                    )
                    .into_styled(PrimitiveStyle::with_fill(bar_color))
                    .draw(display);
                }
                x += bar_max_w as i32 + 2;
            } else if *sty == 8 || *sty == 9 {
                // Inverted badge: draw bg rect then text
                let (bg_color, fg_color) = if *sty == 8 {
                    (Rgb565::GREEN, Rgb565::BLACK)         // ON: green bg
                } else {
                    (Rgb565::new(10, 0, 0), Rgb565::WHITE) // OFF: dark red bg
                };
                let w = text.len() as u32 * CHAR_W as u32;
                let _ = Rectangle::new(Point::new(x, y), Size::new(w, CHAR_H as u32))
                    .into_styled(PrimitiveStyle::with_fill(bg_color))
                    .draw(display);
                let inv_style = MonoTextStyleBuilder::new()
                    .font(&FONT_6X12)
                    .text_color(fg_color)
                    .build();
                let _ = Text::new(text, Point::new(x, y + 10), inv_style).draw(display);
                x += w as i32;
            } else {
                let style = match sty {
                    0 => section_style,
                    1 => label_style,
                    3 => val_yellow,
                    4 => val_on,
                    5 => val_off,
                    6 => val_disabled,
                    _ => val_style,
                };
                let _ = Text::new(text, Point::new(x, y + 10), style).draw(display);
                x += text.len() as i32 * CHAR_W;
            }
        }
    }
    *prev_lines = text_strs;
}

// ─── Terminal page ───────────────────────────────────────────────────────

fn draw_terminal_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    rx_buf: &SerialRxBuffer,
    prev_cells: &mut [[Cell; TERM_COLS]; TERM_ROWS],
    force: bool,
) {
    // Snapshot cells while holding the lock as briefly as possible.
    // The SPI rendering below can take tens of milliseconds; holding the
    // mutex for that entire time blocks push_data() in the serial RX
    // thread and causes receive data loss.
    let (should_redraw, snapshot) = {
        let mut term = rx_buf.inner.lock().unwrap();
        let dirty = term.dirty;
        if dirty { term.dirty = false; }
        (dirty || force, term.cells)
    };

    if !should_redraw {
        return;
    }

    let mut char_buf = [0u8; 4];

    for row in 0..TERM_ROWS {
        for col in 0..TERM_COLS {
            let cell = snapshot[row][col];
            let prev = prev_cells[row][col];
            if !force
                && cell.ch == prev.ch
                && cell.fg == prev.fg
                && cell.bg == prev.bg
            {
                continue;
            }

            let px = col as i32 * CHAR_W;
            let py = HEADER_H + 1 + row as i32 * CHAR_H;

            // Draw background
            let _ = Rectangle::new(
                Point::new(px, py),
                Size::new(CHAR_W as u32, CHAR_H as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(cell.bg))
            .draw(display);

            if cell.ch != ' ' {
                let s = cell.ch.encode_utf8(&mut char_buf);
                let style = MonoTextStyleBuilder::new()
                    .font(&FONT_6X12)
                    .text_color(cell.fg)
                    .build();
                let _ = Text::new(s, Point::new(px, py + 10), style).draw(display);
            }

            prev_cells[row][col] = cell;
        }
    }
}
