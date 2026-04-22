# ssh-serial-bridge

Firmware for the ESP32-S3 that bridges an SSH connection (over Wi-Fi) to UART
and USB CDC serial ports.  Written in Rust (esp-idf-hal / esp-idf-svc) with a
C component (wolfSSH) for the SSH server.

## Features

| Feature | Description |
|---------|-------------|
| SSH server | Port 22, username/password authentication via wolfSSH, one session at a time |
| Serial ports | COM1 / COM2 (UART) and USB CDC (USB Host) — up to 4 ports (`usb0`–`usb3`) |
| Web UI | Browser-based configuration, terminal, and GPIO control |
| WebSocket terminal | Built-in xterm.js; access serial data from the browser at `/terminal` |
| GPIO / PWM | Digital output (ON/OFF) on GPIO 4–9, 12 ; PWM output on GPIO 10–11 |
| SPI display | Optional ST7789V (320×240) display showing incoming serial data |
| UDP Syslog | RFC 5424 remote logging over UDP |
| NTP | Time synchronisation on boot; up to 4 servers configurable via Web UI |
| NVS settings | Persistent configuration stored in NVS; editable via Web UI at runtime |

---

## Architecture

```
SSH client ──[TCP:22]──► ssh_bridge.c (wolfSSH)
                               │
                         ring buffer (PSRAM 128 KB)
                               │
Rust usb_host.rs ◄─────────────┘
 ├── COM1 (UART1)
 ├── COM2 (UART2)
 └── USB CDC (USB Host: PL2303 / FTDI / etc.)

Browser ──[HTTP/WS]──► httpserver.rs
 ├── /terminal  (xterm.js + WebSocket /ws/serial)
 ├── /          (configuration page)
 └── /gpio      (GPIO/PWM control page)
```

---

## Directory Layout

```
ssh-serial-bridge/
├── src/
│   ├── main.rs           # Entry point, peripheral initialisation
│   ├── usb_host.rs       # USB Host (PL2303 etc.) + UART + SSH bridge control
│   ├── httpserver.rs     # HTTP/WebSocket server, NVS config management
│   ├── wifi.rs           # Wi-Fi STA connection (DHCP / static IP)
│   ├── syslogger.rs      # UDP Syslog logger (RFC 5424)
│   ├── serial_display.rs # ST7789V SPI display rendering
│   ├── gpio_ctrl.rs      # Shared GPIO / PWM state
│   ├── btn_ctrl.rs       # GPIO0 button (short press: page cycle; long press: factory reset)
│   └── boot_log.rs       # NVS boot counter and rolling reset-reason log (last 10 entries)
├── components/
│   ├── ssh_bridge/       # SSH server C component (wolfSSH wrapper)
│   ├── wolfssh/          # wolfSSH (github.com/wolfSSL/wolfssh, commit 157cb01f)
│   └── wolfssl/          # wolfSSL/wolfCrypt (github.com/wolfSSL/wolfssl, commit b7e7e755)
├── static/               # Embedded static files (xterm.js / css / fit addon)
├── cfg.toml              # Build-time default configuration (gitignored — contains credentials)
├── cfg.toml.tmp          # Template for cfg.toml (safe to commit)
├── sdkconfig.defaults    # ESP-IDF build settings
├── partitions.csv        # Flash partition table
├── setup_components.sh   # Initial setup script for components/
└── setup_env.sh          # One-shot environment setup (Rust, espup, udev)
```

---

## Pin Assignment

### UART Serial

| Port | Signal | Default GPIO | XIAO-ESP32S3 |
|------|--------|-------------|--------------|
| COM1 | TX     | 17          | 43           |
| COM1 | RX     | 18          | 44           |
| COM2 | TX     | 19 (41)     | —            |
| COM2 | RX     | 20 (42)     | —            |

### GPIO / PWM Output

| GPIO    | Function |
|---------|----------|
| 4–9, 12 | Digital output ON/OFF |
| 10–11   | PWM output (LEDC, 1 kHz, 14-bit) |

### SPI Display (ST7789V)

| Signal | GPIO |
|--------|------|
| SCLK   | 48   |
| MOSI   | 45   |
| CS     | 40   |
| DC     | 39   |
| RST    | 38   |

### Button

| GPIO     | Function |
|----------|----------|
| 0 (BOOT) | Short press: cycle display page / Long press (3 s): NVS factory reset |

---

## Configuration

### cfg.toml (build-time defaults)

Copy `cfg.toml.tmp` to `cfg.toml` and fill in your values before building.

```toml
[ssh-serial-bridge]
wifi_ssid      = "your-ssid"
wifi_psk       = "your-password"
ip_mode        = "dhcp"           # "dhcp" or "static"
ip_address     = "192.168.2.200"
subnet_mask    = "255.255.255.0"
gateway        = "192.168.2.1"
dns            = "1.1.1.1"

syslog_server    = "192.168.2.140:514"
syslog_enable    = "false"
syslog_host_name = "esp32s3"
syslog_app_name  = "ssh-bridge"

ssh_user     = "admin"
ssh_password = "sshpass"

com1_tx_pin = "17"    # XIAO-ESP32S3: 43
com1_rx_pin = "18"    # XIAO-ESP32S3: 44
com1_baud   = "115200"
com2_tx_pin = "41"
com2_rx_pin = "42"
com2_baud   = "115200"

cdc_enable = "false"
cdc_baud   = "115200"

display_enable = "false"
display_port   = "com1"    # "com1" / "com2" / "usb0" / "usb1" / "usb2" / "usb3"

pwm_enable = "true"

# NTP servers (up to 4, in priority order)
ntp_server1 = "time.aws.com"
ntp_server2 = "time.google.com"
ntp_server3 = "time.cloudflare.com"
ntp_server4 = "ntp.nict.jp"
```

Values in `cfg.toml` are compiled into the firmware as defaults.  
After boot they can be overridden via the Web UI or direct NVS writes; NVS values take priority.

---

## Environment Setup

Run `setup_env.sh` **once** on a fresh Ubuntu / Debian machine (or WSL2) before
building.  The script performs all steps below automatically.

```bash
bash setup_env.sh
```

After the script finishes, open a new terminal (or reload your shell config) so
the environment variables take effect:

```bash
source "$HOME/.cargo/env"
source ~/export-esp.sh
```

### What the script does

| Step | Action |
|------|--------|
| 1 | Install system packages via `apt` |
| 2 | Install Rust toolchain via `rustup` (skipped if already present) |
| 3 | Install `ldproxy`, `espup`, `cargo-espflash` via Cargo |
| 4 | Install & update the ESP32-S3 Xtensa toolchain via `espup` |
| 5 | Source `~/export-esp.sh` (generated by `espup`) |
| 6 | Add udev rule for ESP32 USB device (`303a:1001`) and reload rules |

> If you prefer to run the steps manually, expand each step below.

<details>
<summary>Manual setup steps</summary>

```bash
# 1. System packages
sudo apt update && sudo apt -y install \
    git python3 python3-pip gcc build-essential curl \
    pkg-config libudev-dev libtinfo5 clang libclang-dev \
    llvm-dev udev libssl-dev python3.10-venv

# 2. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Select option 1 (default install), then:
source "$HOME/.cargo/env"

# 3. Cargo tools
cargo install ldproxy
cargo install espup
cargo install cargo-espflash

# 4. ESP32-S3 toolchain
espup install
espup update

# 5. Activate ESP environment (add to ~/.bashrc / ~/.zshrc for persistence)
. ~/export-esp.sh

# 6. udev rules
sudo sh -c 'echo "SUBSYSTEMS==\"usb\", ATTRS{idVendor}==\"303a\", ATTRS{idProduct}==\"1001\", MODE=\"0666\"" > /etc/udev/rules.d/99-esp32.rules'
sudo udevadm control --reload-rules
sudo udevadm trigger
```

</details>

---

## Build and Flash

### Prerequisites

- Rust toolchain: `esp` channel (selected automatically via `rust-toolchain.toml`)
- [espup](https://github.com/esp-rs/espup) — ESP32-S3 Xtensa toolchain
- [espflash](https://github.com/esp-rs/espflash)

### Initial component setup

Run once after cloning, or after a clean wipe of `components/`:

```bash
cd esp32-s3/ssh-serial-bridge
bash setup_components.sh
```

This clones wolfssh and wolfssl at the verified commits and places the
ESP-IDF-specific `CMakeLists.txt` and `include/user_settings.h` on top.

### Build

```bash
cargo build --release
```

### Flash and monitor

```bash
espflash flash --release --monitor
```

### Flash layout

| Partition     | Offset  | Size   |
|---------------|---------|--------|
| nvs           | 0x9000  | 24 KB  |
| phy_init      | 0xF000  | 4 KB   |
| factory (app) | 0x10000 | 7.9 MB |

Flash: 8 MB, DIO mode, 40 MHz.

---

## Web UI Endpoints

| URL | Method | Description |
|-----|--------|-------------|
| `/login` | GET / POST | Login |
| `/logout` | GET | Logout |
| `/` | GET | Configuration page (auth required) |
| `/gpio` | GET | GPIO / PWM control page (auth required) |
| `/terminal` | GET | WebSocket serial terminal (auth required) |
| `/ws/serial` | WS | Bidirectional serial bridge |
| `/api/config` | POST | Save configuration to NVS |
| `/api/status` | GET | Device / Wi-Fi / USB status (JSON) |
| `/api/reset` | POST | Erase NVS and restore cfg.toml defaults |
| `/api/reboot` | POST | Reboot the device |
| `/api/gpio` | POST | Set one digital output ON/OFF |
| `/api/pwm` | POST | Set PWM duty cycle (0–100 %) |
| `/api/gpio_state` | GET | Current GPIO / PWM state (JSON) |
| `/api/boot_log` | GET | Boot history with reset reasons from NVS (JSON) |

---

## SSH Connection

### Authentication

| Item | Detail |
|------|--------|
| Port | 22 (TCP) |
| Auth method | Password only (public-key authentication is not supported) |
| Username | Any string is accepted — only the password is checked |
| Password | Set via `ssh_password` in `cfg.toml` or Web UI (default: `esp32`) |
| Concurrent sessions | 1 (a second connection is rejected while a session is active) |

> **Security note:** Change the default password before deploying the device
> on a shared network.

---

### Commands

The SSH server dispatches on the command passed after the host.  
If no command is given, a usage summary is printed and the connection closes.

#### 1. Serial console bridge

Connects the SSH session as a transparent terminal to a serial port.

```bash
ssh -tt <user>@<device_ip> console (usb0|usb1|usb2|usb3|com1|com2)
```

| Argument | Description |
|----------|-------------|
| `usb0`   | First USB CDC device (plug order; alias: `usb`) |
| `usb1`   | Second USB CDC device |
| `usb2`   | Third USB CDC device |
| `usb3`   | Fourth USB CDC device |
| `com1`   | UART1 (pins configured by `com1_tx_pin` / `com1_rx_pin`) |
| `com2`   | UART2 (pins configured by `com2_tx_pin` / `com2_rx_pin`) |

- The `-tt` flag forces pseudo-TTY allocation, which is required for correct
  terminal behaviour.
- A welcome banner is printed on connect:
  ```
  Connected to ESP32-S3 serial bridge [com1].
  Disconnect: Enter then ~.
  ```
- Serial data received before the first keystroke from the SSH client is
  discarded to prevent stale output flooding the terminal on login.
- Disconnect with the standard OpenSSH escape sequence: press **Enter**, then
  type **`~.`**

**Examples:**

```bash
# Connect to the first USB CDC device
ssh -tt admin@192.168.2.200 console usb0

# Connect to the second USB CDC device
ssh -tt admin@192.168.2.200 console usb1

# Connect to COM1 (UART1)
ssh -tt admin@192.168.2.200 console com1

# Connect to COM2 (UART2)
ssh -tt admin@192.168.2.200 console com2
```

---

#### 2. GPIO digital output control

Controls GPIO4–GPIO9 output pins (mapped as power outlets 1–6).

```bash
ssh <user>@<device_ip> power <1-6> (on|off)
```

| Argument | Description |
|----------|-------------|
| `1`–`6`  | Outlet number: 1 = GPIO4, 2 = GPIO5, … 6 = GPIO9 |
| `on`     | Set output HIGH |
| `off`    | Set output LOW  |

**Examples:**

```bash
ssh admin@192.168.2.200 power 1 on    # GPIO4 → HIGH
ssh admin@192.168.2.200 power 3 off   # GPIO6 → LOW
```

---

#### 3. DC Power output control (GPIO12)

Controls the dedicated DC Power output on GPIO12.

```bash
ssh <user>@<device_ip> dcpower (on|off)
```

**Examples:**

```bash
ssh admin@192.168.2.200 dcpower on
ssh admin@192.168.2.200 dcpower off
```

---

#### 4. PWM duty cycle control

Sets the PWM duty cycle on GPIO10 or GPIO11 (LEDC, 1 kHz, 14-bit).

```bash
ssh <user>@<device_ip> pwm <1|2> <0-100>
```

| Argument | Description |
|----------|-------------|
| `1`      | Channel 1 — GPIO10 |
| `2`      | Channel 2 — GPIO11 |
| `0`–`100`| Duty cycle in percent |

**Examples:**

```bash
ssh admin@192.168.2.200 pwm 1 50    # GPIO10 → 50 %
ssh admin@192.168.2.200 pwm 2 0     # GPIO11 → off
ssh admin@192.168.2.200 pwm 1 100   # GPIO10 → full on
```

---

### Usage summary (printed on unknown command)

```
ESP32-S3 SSH-Serial-Bridge
Usage:
  ssh -tt admin@host console (usb|com1|com2)  -- Serial console bridge
  ssh    admin@host power   (1-6) (on|off)    -- GPIO4-9 output control
  ssh    admin@host dcpower (on|off)          -- DC Power output (GPIO12)
  ssh    admin@host pwm   (1|2) (0-100)       -- PWM GPIO10-11 duty %
```

---

## Dependencies

| Component / Library | Version | License | Purpose |
|---------------------|---------|---------|---------|
| **ssh-serial-bridge** (this project) | 0.1.0 | MIT | Firmware (Rust) |
| **ssh_bridge** component | 0.1.0 | MIT | SSH server C wrapper |
| esp-idf-hal | 0.45.2 | MIT / Apache-2.0 | ESP32-S3 hardware abstraction |
| esp-idf-svc | 0.51 | MIT / Apache-2.0 | Wi-Fi, HTTP server, SNTP, etc. |
| **wolfSSH** | commit `157cb01f` | GPLv3 (or commercial) | SSH server protocol |
| **wolfSSL / wolfCrypt** | commit `b7e7e755` | GPLv3 (or commercial) | Cryptography library |
| mipidsi | 0.8.0 | MIT | ST7789V display driver |
| embedded-graphics | 0.8.1 | MIT | Display rendering |
| @xterm/xterm | 6.0.0 (bundled) | MIT | Browser WebSocket terminal |
| @xterm/addon-fit | 0.11.0 (bundled) | MIT | xterm.js terminal fit addon |

> **Note on wolfSSH / wolfSSL licensing:**  
> Both wolfSSH and wolfSSL are dual-licensed under **GPLv3** (open source) or a
> **commercial license** (for use in proprietary products).  
