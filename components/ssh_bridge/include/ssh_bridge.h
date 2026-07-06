/* ssh_bridge.h
 *
 * SSH-to-USB-CDC serial bridge for ESP32-S3.
 *
 * The bridge listens for SSH connections on a TCP port.  Each session
 * presents a terminal whose I/O is directly coupled to the USB CDC
 * (serial) device attached via USB OTG host.
 *
 * Thread safety:
 *   - ssh_bridge_init()  must be called once before tasks start.
 *   - ssh_bridge_cdc_rx() may be called from any task/ISR context.
 */

#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Callback invoked when the SSH client sends data that should be forwarded
 * to the USB CDC device (USB bulk-OUT).
 *
 * @param data  pointer to the bytes received from SSH client
 * @param len   number of bytes
 */
typedef void (*ssh_cdc_tx_cb_t)(const uint8_t *data, size_t len);

/**
 * Initialise and start the SSH bridge.
 *
 * Spawns two FreeRTOS tasks:
 *   - SSH server listener (TCP accept loop)
 *   - SSH session handler (SSH protocol + data forwarding)
 *
 * @param port        TCP port to listen on (typically 22)
 * @param username    accepted username (currently informational; any name is accepted)
 * @param password    accepted password (NULL falls back to compile-time default)
 */
int ssh_bridge_init(uint16_t port,
                    const char *username, const char *password);

/**
 * Feed data received from a serial device (USB CDC bulk-IN or UART RX) into
 * the SSH bridge.  The data is fanned out into the ring buffer of every SSH
 * session currently bridging that device, so all clients connected to the
 * same device see the same serial stream (multiple concurrent sessions per
 * device are supported, up to MAX_SESSIONS_PER_DEVICE).
 *
 * Safe to call from an interrupt or any FreeRTOS task.
 *
 * @param device_id  device identifier (must match the ID scheme used by the
 *                    Rust-side DEVICE_* constants: 1=usb0, 2=com1, 3=com2,
 *                    4=usb1, 5=usb2, 6=usb3)
 * @param data       received bytes
 * @param len        number of bytes
 */
void ssh_bridge_cdc_rx(uint8_t device_id, const uint8_t *data, size_t len);

#ifdef __cplusplus
}
#endif
