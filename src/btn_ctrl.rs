// Button control thread — GPIO0 (BOOT button, active-low, internal pull-up)
//
// Short press      (< 3 s):       cycle display page  (no-op when display is disabled)
// Medium press     (3 s ~ 10 s):  toggle DC OUT (GPIO12) ON/OFF
// Very long press  (≥ 10 s):      erase NVS and restart → restores cfg.toml defaults

use std::thread;
use std::time::{Duration, Instant};
use log::info;
use esp_idf_hal::gpio::{Gpio0, PinDriver, Pull};
use crate::gpio_ctrl::GpioPwmState;

/// Duration in milliseconds for DC OUT toggle (3 seconds).
const DCOUT_PRESS_MS: u64 = 3000;

/// Duration in milliseconds for a long-press factory reset (10 seconds).
const FACTORY_RESET_PRESS_MS: u64 = 10000;

/// Spawn the dedicated button-control thread.
/// This is always started regardless of whether the display is enabled.
pub fn start_button_thread(btn_pin: Gpio0, gpio_pwm_state: GpioPwmState) {
    thread::Builder::new()
        .name("btn_ctrl".into())
        .stack_size(4096)
        .spawn(move || button_task(btn_pin, gpio_pwm_state))
        .expect("spawn btn_ctrl thread");
}

fn button_task(btn_pin: Gpio0, gpio_pwm_state: GpioPwmState) {
    let mut btn = PinDriver::input(btn_pin).unwrap();
    btn.set_pull(Pull::Up).unwrap();

    let mut btn_last = true;           // true = released (pin high)
    let mut press_start: Option<Instant> = None;
    let mut dcout_triggered = false;
    let mut factory_reset_triggered = false;

    loop {
        let btn_now = btn.is_high(); // high = released

        if !btn_now && btn_last {
            // ── Falling edge: button just pressed ───────────────────────
            press_start = Some(Instant::now());
            dcout_triggered = false;
            factory_reset_triggered = false;
        } else if !btn_now {
            // ── Button held ─────────────────────────────────────────────
            if let Some(start) = press_start {
                let elapsed = start.elapsed().as_millis() as u64;
                
                // Check for factory reset (≥10s)
                if !factory_reset_triggered && elapsed >= FACTORY_RESET_PRESS_MS {
                    info!("GPIO0 very long press ({}ms) — factory reset to cfg.toml defaults", elapsed);
                    crate::httpserver::factory_reset(); // never returns (no need to set flag)
                }
                // Check for DC OUT toggle (≥3s but <10s)
                else if !dcout_triggered && elapsed >= DCOUT_PRESS_MS {
                    dcout_triggered = true;
                    let current_state = gpio_pwm_state.get_gpio()[6]; // index 6 = GPIO12 = DC OUT
                    let new_state = !current_state;
                    gpio_pwm_state.set_gpio(6, new_state);
                    info!("GPIO0 medium press ({}ms) — toggling DC OUT (GPIO12): {} -> {}", 
                          elapsed, current_state, new_state);
                }
            }
        } else if btn_now && !btn_last {
            // ── Rising edge: button released ─────────────────────────────
            if !dcout_triggered && !factory_reset_triggered {
                if let Some(start) = press_start.take() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    info!("GPIO0 short press ({}ms) — next display page", elapsed);
                    crate::serial_display::next_page();
                }
            }
            press_start = None;
            dcout_triggered = false;
            factory_reset_triggered = false;
        }

        btn_last = btn_now;
        thread::sleep(Duration::from_millis(50));
    }
}
