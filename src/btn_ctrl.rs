// Button control thread — GPIO0 (BOOT button, active-low, internal pull-up)
//
// Short press  (< 3 s): cycle display page  (no-op when display is disabled)
// Long press   (≥ 3 s): erase NVS and restart → restores cfg.toml defaults

use std::thread;
use std::time::{Duration, Instant};
use log::info;
use esp_idf_hal::gpio::{Gpio0, PinDriver, Pull};

/// Duration in milliseconds for a long-press factory reset.
const LONG_PRESS_MS: u64 = 3000;

/// Spawn the dedicated button-control thread.
/// This is always started regardless of whether the display is enabled.
pub fn start_button_thread(btn_pin: Gpio0) {
    thread::Builder::new()
        .name("btn_ctrl".into())
        .stack_size(4096)
        .spawn(move || button_task(btn_pin))
        .expect("spawn btn_ctrl thread");
}

fn button_task(btn_pin: Gpio0) {
    let mut btn = PinDriver::input(btn_pin).unwrap();
    btn.set_pull(Pull::Up).unwrap();

    let mut btn_last = true;           // true = released (pin high)
    let mut press_start: Option<Instant> = None;
    let mut long_press_triggered = false;

    loop {
        let btn_now = btn.is_high(); // high = released

        if !btn_now && btn_last {
            // ── Falling edge: button just pressed ───────────────────────
            press_start = Some(Instant::now());
            long_press_triggered = false;
        } else if !btn_now {
            // ── Button held ─────────────────────────────────────────────
            if !long_press_triggered {
                if let Some(start) = press_start {
                    if start.elapsed().as_millis() as u64 >= LONG_PRESS_MS {
                        info!("GPIO0 long press ({}ms) — factory reset to cfg.toml defaults",
                              start.elapsed().as_millis());
                        crate::httpserver::factory_reset(); // never returns
                    }
                }
            }
        } else if btn_now && !btn_last {
            // ── Rising edge: button released ─────────────────────────────
            if !long_press_triggered {
                if let Some(start) = press_start.take() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    info!("GPIO0 short press ({}ms) — next display page", elapsed);
                    crate::serial_display::next_page();
                }
            }
            press_start = None;
            long_press_triggered = false;
        }

        btn_last = btn_now;
        thread::sleep(Duration::from_millis(50));
    }
}
