// GPIO / PWM control state shared between main loop and HTTP server
// GPIO 4-9  : digital output ON/OFF
// GPIO 10-11: PWM (LEDC) duty 0-100 %
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

/// GPIO index → physical pin number mapping.
/// Index 0-5 = GPIO4-9, index 6 = GPIO12.
pub const GPIO_PIN_MAP: [u8; 7] = [4, 5, 6, 7, 8, 9, 12];

/// Shared state: desired GPIO output values.
/// The main thread owns the hardware drivers; this struct carries only the
/// desired values that the HTTP handlers write and the main loop reads.
#[derive(Clone)]
pub struct GpioPwmState {
    /// ON/OFF for GPIO 4-9, 12.  Index 0 = GPIO4 … index 5 = GPIO9, index 6 = GPIO12.
    pub gpio_states: Arc<Mutex<[bool; 7]>>,
    /// PWM duty 0-100 % for GPIO 10-11.  Index 0 = GPIO10, index 1 = GPIO11.
    pub pwm_duties: Arc<Mutex<[u8; 2]>>,
}

impl GpioPwmState {
    pub fn new() -> Self {
        Self {
            gpio_states: Arc::new(Mutex::new([false; 7])),
            pwm_duties:  Arc::new(Mutex::new([0u8; 2])),
        }
    }

    /// Return a snapshot of pin states (thread-safe copy).
    pub fn get_gpio(&self) -> [bool; 7] {
        *self.gpio_states.lock().unwrap()
    }

    /// Return a snapshot of PWM duties (thread-safe copy).
    pub fn get_pwm(&self) -> [u8; 2] {
        *self.pwm_duties.lock().unwrap()
    }

    /// Set one GPIO output (index 0-5 → GPIO4-9, index 6 → GPIO12).
    pub fn set_gpio(&self, index: usize, value: bool) {
        if index < 7 {
            self.gpio_states.lock().unwrap()[index] = value;
        }
    }

    /// Set one PWM duty (index 0-1 → GPIO10-11, value 0-100).
    pub fn set_pwm(&self, index: usize, duty: u8) {
        if index < 2 {
            self.pwm_duties.lock().unwrap()[index] = duty.min(100);
        }
    }
}
