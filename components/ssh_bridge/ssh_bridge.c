/* ssh_bridge.c
 *
 * SSH server that bridges an SSH terminal session to a USB CDC serial
 * device.
 *
 * Author:  Hiroshi Nakajima
 * 
 * Version 0.1.0 (2026-04-15 initial release)
 * 
 * Credentials (compile-time defaults):
 *   username : any
 *   password : "esp32"   (change SSH_BRIDGE_PASSWORD as needed)
 *
 * Architecture:
 *   Listener task  →  accept TCP connections
 *   Session task   →  one at a time; handles wolfSSH protocol + data
 *
 *   CDC → SSH :  ssh_bridge_cdc_rx() feeds a ring buffer;
 *                session task drains it via wolfSSH_ChannelIdSend().
 *   SSH → CDC :  session task reads via wolfSSH_ChannelIdRead();
 *                calls the cdc_tx_cb provided at init.
 */

#include "ssh_bridge.h"

/* wolfSSL / wolfCrypt */
#define WOLFSSL_USER_SETTINGS
#include <wolfssl/wolfcrypt/settings.h>
#include <wolfssl/wolfcrypt/sha256.h>
#include <wolfssl/wolfcrypt/wc_port.h>

/* wolfSSH */
#include <wolfssh/ssh.h>
#include <wolfssh/certs_test.h>  /* embedded RSA host key buffer */
#include <wolfssh/error.h>

/* ESP-IDF */
#include <esp_log.h>
#include <esp_heap_caps.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>
#include <freertos/semphr.h>
#include <freertos/idf_additions.h>

/* POSIX sockets */
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>  /* malloc / free / realloc for wolfSSL_SetAllocators */

/* ── Configuration ───────────────────────────────────────────────────────── */

#define TAG                 "SSH_BRIDGE"

#ifndef SSH_BRIDGE_PASSWORD
#define SSH_BRIDGE_PASSWORD "esp32"
#endif

/* Ring buffer size for CDC → SSH direction.
 * Must be large enough to absorb bursts while the SSH channel window
 * is temporarily full.  USB CDC data arrives at USB bus speed (not
 * baud-rate limited like UART), so commands like `dmesg` via g_serial
 * can dump tens of KB in a single burst.  128 KB in PSRAM is safe. */
#define CDC_RING_SZ  (128 * 1024)

/* wolfSSH session task stack size.
 * wolfSSH_accept() runs RSA key exchange which consumes deep stack;
 * non-blocking wolfSSH_worker() may also recurse.  48 KB is safe. */
#define SSH_SESSION_STACK  (48 * 1024)

/* wolfSSH listener task stack size */
#define SSH_LISTEN_STACK   (6 * 1024)

/* Shell channel ID (first channel opened is always 0) */
#define SHELL_CHANNEL_ID   0

/* ── CDC → SSH ring buffer ───────────────────────────────────────────────── */

static uint8_t          *s_ring      = NULL;   /* allocated from PSRAM */
static volatile uint32_t s_ring_head = 0;
static volatile uint32_t s_ring_tail = 0;
static SemaphoreHandle_t s_ring_mutex = NULL;

/* Write bytes into the ring buffer (overflow = silently dropped) */
static void ring_write(const uint8_t *data, size_t len)
{
    if (!s_ring_mutex || !s_ring) return;
    xSemaphoreTake(s_ring_mutex, portMAX_DELAY);
    for (size_t i = 0; i < len; i++) {
        uint32_t next = (s_ring_head + 1) % CDC_RING_SZ;
        if (next != s_ring_tail) {
            s_ring[s_ring_head] = data[i];
            s_ring_head = next;
        }
    }
    xSemaphoreGive(s_ring_mutex);
}

/* Drain up to max_len bytes from the ring buffer.  Returns count read. */
static size_t ring_read(uint8_t *buf, size_t max_len)
{
    if (!s_ring_mutex || !s_ring) return 0;
    xSemaphoreTake(s_ring_mutex, portMAX_DELAY);
    size_t n = 0;
    while (s_ring_tail != s_ring_head && n < max_len) {
        buf[n++] = s_ring[s_ring_tail];
        s_ring_tail = (s_ring_tail + 1) % CDC_RING_SZ;
    }
    xSemaphoreGive(s_ring_mutex);
    return n;
}

/* ── Shared state ────────────────────────────────────────────────────────── */

static uint16_t         s_port     = 22;
static const char      *s_password = NULL;  /* set by ssh_bridge_init */

/* One active session at a time; protected by mutex */
static volatile WOLFSSH *s_active_ssh = NULL;
static SemaphoreHandle_t s_ssh_mutex  = NULL;

/* wolfSSH context (shared across sessions) */
static WOLFSSH_CTX *s_ctx = NULL;

/* ── wolfSSH authentication callback ─────────────────────────────────────── */

static int ws_user_auth(byte authType,
                        WS_UserAuthData *authData,
                        void *ctx)
{
    (void)ctx;

    if (authType != WOLFSSH_USERAUTH_PASSWORD)
        return WOLFSSH_USERAUTH_FAILURE;

    const char *pw  = s_password ? s_password : SSH_BRIDGE_PASSWORD;
    word32      pwz = (word32)strlen(pw);

    if (authData->sf.password.passwordSz == pwz &&
        memcmp(authData->sf.password.password, pw, pwz) == 0) {
        ESP_LOGI(TAG, "SSH auth OK for user '%.*s'",
                 (int)authData->usernameSz, authData->username);
        return WOLFSSH_USERAUTH_SUCCESS;
    }

    ESP_LOGW(TAG, "SSH auth FAILED for user '%.*s'",
             (int)authData->usernameSz, authData->username);
    return WOLFSSH_USERAUTH_FAILURE;
}

/* ── Rust-side logging (goes through syslog) ─────────────────────────────── */
extern void ssh_log_info(const char *msg);
extern void ssh_log_warn(const char *msg);
extern void ssh_log_hex(const char *dir, const uint8_t *data, size_t len);
extern const char *ssh_device_error(void);
extern ssh_cdc_tx_cb_t ssh_select_device(const char *name);
extern void ssh_release_device(const char *name);

/* ── SSH session handler task ────────────────────────────────────────────── */

/* ── Rust-side GPIO / PWM callbacks ──────────────────────────────────────── */
extern int ssh_gpio_set(int idx, int on_off);
extern int ssh_pwm_set(int ch, int duty);

static void ssh_session_task(void *arg)
{
    int             client_fd      = (int)(intptr_t)arg;
    char            device_name[32] = {0};     /* filled only for console sessions */
    ssh_cdc_tx_cb_t device_write_cb = NULL;

    /* Build wolfSSH session */
    ESP_LOGI(TAG, "heap: total=%u internal=%u psram=%u",
             (unsigned)esp_get_free_heap_size(),
             (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
             (unsigned)heap_caps_get_free_size(MALLOC_CAP_SPIRAM));

    xSemaphoreTake(s_ssh_mutex, portMAX_DELAY);

    WOLFSSH *ssh = wolfSSH_new(s_ctx);
    if (!ssh) {
        ESP_LOGE(TAG, "wolfSSH_new failed — internal free=%u",
                 (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL));
        xSemaphoreGive(s_ssh_mutex);
        close(client_fd);
        vTaskDelete(NULL);
        return;
    }
    wolfSSH_set_fd(ssh, client_fd);
    s_active_ssh = ssh;

    xSemaphoreGive(s_ssh_mutex);

    ESP_LOGI(TAG, "heap after wolfSSH_new: total=%u internal=%u",
             (unsigned)esp_get_free_heap_size(),
             (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL));

    /* SSH handshake (blocking — wolfSSH_accept does not handle EAGAIN) */
    int rc = wolfSSH_accept(ssh);
    if (rc != WS_SUCCESS) {
        ESP_LOGW(TAG, "wolfSSH_accept error: rc=%d err=%d  internal_free=%u",
                 rc, wolfSSH_get_error(ssh),
                 (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL));
        goto cleanup;
    }

    /* ── Parse exec command ──────────────────────────────────────────────────
     * Expected forms:
     *   ssh <host> console (usb|com1|com2)   – serial bridge
     *   ssh <host> power   (1-6) (on|off)    – GPIO 4-9 output
     *   ssh <host> dcpower (on|off)           – DC Power (GPIO12)
     *   ssh <host> pwm     (1|2) (0-100)     – PWM GPIO 10-11
     * ─────────────────────────────────────────────────────────────────────── */
    char cmd[16] = {0};
    char a1[32]  = {0};
    char a2[32]  = {0};
    {
        const char *exec_cmd = wolfSSH_GetSessionCommand(ssh);
        if (exec_cmd && exec_cmd[0] != '\0') {
            sscanf(exec_cmd, "%15s %31s %31s", cmd, a1, a2);
        }
        ESP_LOGI(TAG, "SSH exec: cmd='%s' a1='%s' a2='%s'", cmd, a1, a2);
    }

    /* ── Dispatch ─────────────────────────────────────────────────────────── */

    if (strcmp(cmd, "console") == 0) {
        /* ssh -tt <host> console (usb|com1|com2) */
        snprintf(device_name, sizeof(device_name), "%s", a1[0] ? a1 : "usb");

        device_write_cb = ssh_select_device(device_name);
        if (!device_write_cb) {
            const char *reason = ssh_device_error();
            char err[256];
            snprintf(err, sizeof(err),
                     "Error: %s\r\n"
                     "Usage: ssh -tt admin@host console (usb|com1|com2)\r\n",
                     reason);
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)err, (word32)strlen(err));
            goto cleanup;
        }

        /* Welcome banner */
        {
            char banner[256];
            snprintf(banner, sizeof(banner),
                     "\r\nConnected to ESP32-S3 serial bridge [%s].\r\n"
                     "Disconnect: Enter then ~.\r\n\r\n",
                     device_name);
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)banner, (word32)strlen(banner));
        }

        /* Discard any serial data that accumulated before or during connect,
         * so stale output does not flood the terminal on login. */
        {
            uint8_t tmp[256];
            size_t flushed = 0;
            size_t r;
            while ((r = ring_read(tmp, sizeof(tmp))) > 0) flushed += r;
            // char msg[64];
            // snprintf(msg, sizeof(msg), "[DBG] ring flushed %d bytes on connect", (int)flushed);
            // ssh_log_info(msg);
        }

        /* ── Data bridge loop ──────────────────────────────────────────────────────────
         * select() with 10 ms timeout: runs wolfSSH_worker() only when SSH
         * socket has data, otherwise only drains the serial→SSH ring buffer.
         *
         * Pending-send mechanism: wolfSSH_ChannelIdSend() may return a partial
         * byte count or WS_WANT_WRITE when the SSH channel window is full.
         * We keep the unsent remainder in `pend_buf` and retry on the next
         * iteration *before* reading more from the ring buffer.
         *
         * input_received: serial→SSH forwarding is suppressed until the SSH
         * client sends at least one byte (prevents stale data appearing on
         * connect before the user has interacted with the terminal).
         * ──────────────────────────────────────────────────────────────────── */
        {
            /* Set socket non-blocking so wolfSSH_worker() returns
             * WS_WANT_READ immediately instead of blocking the loop. */
            int flags = fcntl(client_fd, F_GETFL, 0);
            fcntl(client_fd, F_SETFL, flags | O_NONBLOCK);

            uint8_t buf[512];
            word32  lastChannel = 0;
            int     input_received = 0; /* 0 = suppress serial→SSH until first SSH TX */

            /* Pending-send state: data that was ring_read()'d but could not
             * yet be pushed through wolfSSH_ChannelIdSend(). */
            uint8_t pend_buf[512];
            size_t  pend_off = 0;   /* offset of first unsent byte */
            size_t  pend_len = 0;   /* total valid bytes in pend_buf */

            while (1) {
                fd_set rfds, wfds;
                struct timeval tv = { .tv_sec = 0, .tv_usec = 10000 };
                FD_ZERO(&rfds);
                FD_ZERO(&wfds);
                FD_SET(client_fd, &rfds);
                /* When data is waiting to be sent, also poll for write
                 * readiness so wolfSSH_worker can flush to TCP. */
                if (pend_len > 0)
                    FD_SET(client_fd, &wfds);
                int sel = select(client_fd + 1, &rfds, &wfds, NULL, &tv);
                if (sel < 0) {
                    ESP_LOGI(TAG, "select() error: errno=%d", errno);
                    break;
                }

                /* Always pump the SSH protocol so outgoing data gets
                 * flushed to TCP — not just when the socket is readable.
                 * wolfSSH_worker() handles non-blocking correctly and
                 * returns WS_WANT_READ when there is nothing to receive. */
                rc = wolfSSH_worker(ssh, &lastChannel);
                int wolf_err = wolfSSH_get_error(ssh);

                if (rc == WS_CHAN_RXD && lastChannel == SHELL_CHANNEL_ID) {
                    /* SSH client → device TX.
                     * Drain wolfSSH's internal channel buffer completely:
                     * a single large paste may be buffered internally after
                     * one wolfSSH_worker() call, but wolfSSH_ChannelIdRead()
                     * returns at most sizeof(buf) bytes per call.  Without
                     * this loop the remainder sits in the wolfSSH buffer until
                     * the next WS_CHAN_RXD, so the device never receives the
                     * full paste and the echo never comes back. */
                    int n;
                    while ((n = wolfSSH_ChannelIdRead(ssh, SHELL_CHANNEL_ID,
                                                       buf, sizeof(buf))) > 0) {
                        if (device_write_cb) {
                            device_write_cb(buf, (size_t)n);
                            if (!input_received) {
                                /* Discard any serial data queued before first input */
                                uint8_t discard[256];
                                size_t  dr;
                                while ((dr = ring_read(discard, sizeof(discard))) > 0)
                                    (void)dr;
                                input_received = 1; /* unlock serial→SSH forwarding */
                            }
                        }
                    }
                    if (n < 0 && n != WS_WANT_READ) {
                        ESP_LOGI(TAG, "ChannelIdRead err %d", n);
                        break;
                    }
                } else if (rc == WS_SUCCESS   ||
                           rc == WS_WANT_READ  ||
                           rc == WS_WANT_WRITE ||
                           rc == WS_CHAN_RXD    ||
                           wolf_err == WS_WANT_READ  ||
                           wolf_err == WS_WANT_WRITE) {
                    /* Normal transient states (non-blocking socket) */
                } else {
                    // { char tmp[96];
                    //   snprintf(tmp, sizeof(tmp), "[DBG] loop break rc=%d err=%d", rc, wolf_err);
                    //   ssh_log_info(tmp); }
                    ESP_LOGI(TAG, "wolfSSH_worker ended rc=%d err=%d",
                             rc, wolf_err);
                    break;
                }

                /* Device RX → SSH client (with retry for partial sends) */

                /* First, flush any pending unsent data from previous iteration */
                while (pend_off < pend_len) {
                    int w = wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                                   pend_buf + pend_off,
                                                   (word32)(pend_len - pend_off));
                    if (w > 0) {
                        pend_off += (size_t)w;
                    } else if (w == WS_WANT_WRITE || w == 0) {
                        break;  /* channel window full — retry next iteration */
                    } else {
                        ESP_LOGI(TAG, "ChannelIdSend err %d", w);
                        goto cleanup;
                    }
                }
                if (pend_off >= pend_len) {
                    pend_off = 0;
                    pend_len = 0;
                }

                /* If nothing pending, read fresh data from the ring buffer.
                 * Discard silently until the SSH client has sent its first
                 * keystroke so the terminal is not flooded on connect. */
                if (pend_len == 0) {
                    size_t serial_n = ring_read(buf, sizeof(buf));
                    // if (serial_n > 0) {
                    //     char tmp[96];
                    //     snprintf(tmp, sizeof(tmp), "[DBG] ring_read %d bytes input_received=%d [0x%02x]",
                    //              (int)serial_n, input_received, buf[0]);
                    //     ssh_log_info(tmp);
                    // }
                    if (serial_n > 0 && !input_received) {
                        // char tmp[64];
                        // snprintf(tmp, sizeof(tmp), "[DBG] DISCARDED %d bytes (no input yet)", (int)serial_n);
                        // ssh_log_info(tmp);
                        serial_n = 0;
                    }
                    if (serial_n > 0) {
                        int w = wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                                       buf, (word32)serial_n);
                        if (w > 0 && (size_t)w < serial_n) {
                            /* Partial write — save remainder */
                            pend_len = serial_n;
                            pend_off = (size_t)w;
                            memcpy(pend_buf, buf, serial_n);
                        } else if (w == WS_WANT_WRITE || w == 0) {
                            /* Channel window full — save entire chunk */
                            pend_len = serial_n;
                            pend_off = 0;
                            memcpy(pend_buf, buf, serial_n);
                        } else if (w < 0 && w != WS_WANT_WRITE) {
                            ESP_LOGI(TAG, "ChannelIdSend err %d", w);
                            break;
                        }
                    }
                }
            }
        }

    } else if (strcmp(cmd, "power") == 0) {
        /* ssh <host> power (1-6) (on|off) */
        int pin = atoi(a1);
        int on  = (strcmp(a2, "on") == 0) ? 1 : 0;

        if (pin < 1 || pin > 6 ||
            (strcmp(a2, "on") != 0 && strcmp(a2, "off") != 0)) {
            const char *usage =
                "Usage: ssh admin@host power (1-6) (on|off)\r\n"
                "  Maps to GPIO4-GPIO9 (power 1 = GPIO4, ... power 6 = GPIO9)\r\n";
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)usage, (word32)strlen(usage));
        } else {
            int r = ssh_gpio_set(pin - 1, on);
            char resp[64];
            if (r == 0) {
                snprintf(resp, sizeof(resp),
                         "GPIO%d (power %d) -> %s\r\n",
                         pin + 3, pin, on ? "ON" : "OFF");
            } else {
                snprintf(resp, sizeof(resp), "Error: GPIO set failed\r\n");
            }
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)resp, (word32)strlen(resp));
        }
        goto cleanup;

    } else if (strcmp(cmd, "dcpower") == 0) {
        /* ssh <host> dcpower (on|off)  — DC Power output (GPIO12, index 6) */
        int on = (strcmp(a1, "on") == 0) ? 1 : 0;

        if (strcmp(a1, "on") != 0 && strcmp(a1, "off") != 0) {
            const char *usage =
                "Usage: ssh admin@host dcpower (on|off)\r\n"
                "  Controls DC Power output (GPIO12)\r\n";
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)usage, (word32)strlen(usage));
        } else {
            int r = ssh_gpio_set(6, on);
            char resp[64];
            if (r == 0) {
                snprintf(resp, sizeof(resp),
                         "DCPOWER (GPIO12) -> %s\r\n", on ? "ON" : "OFF");
            } else {
                snprintf(resp, sizeof(resp), "Error: DCPOWER set failed\r\n");
            }
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)resp, (word32)strlen(resp));
        }
        goto cleanup;

    } else if (strcmp(cmd, "pwm") == 0) {
        /* ssh <host> pwm (1|2) (0-100) */
        int ch   = atoi(a1);
        int duty = atoi(a2);

        if (ch < 1 || ch > 2 || duty < 0 || duty > 100) {
            const char *usage =
                "Usage: ssh admin@host pwm (1|2) (0-100)\r\n"
                "  ch 1 = GPIO10, ch 2 = GPIO11\r\n";
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)usage, (word32)strlen(usage));
        } else {
            int r = ssh_pwm_set(ch - 1, duty);
            char resp[64];
            if (r == 0) {
                snprintf(resp, sizeof(resp),
                         "PWM%d (GPIO%d) -> %d%%\r\n",
                         ch, ch + 9, duty);
            } else {
                snprintf(resp, sizeof(resp), "Error: PWM set failed\r\n");
            }
            wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                                   (const byte *)resp, (word32)strlen(resp));
        }
        goto cleanup;

    } else {
        /* No command or unknown command → show usage */
        const char *usage =
            "\r\nESP32-S3 SSH-Serial-Bridge\r\n"
            "Usage:\r\n"
            "  ssh -tt admin@host console (usb|com1|com2)  -- Serial console bridge\r\n"
            "  ssh    admin@host power   (1-6) (on|off)    -- GPIO4-9 output control\r\n"
            "  ssh    admin@host dcpower (on|off)          -- DC Power output (GPIO12)\r\n"
            "  ssh    admin@host pwm   (1|2) (0-100)       -- PWM GPIO10-11 duty %%\r\n"
            "\r\n";
        wolfSSH_ChannelIdSend(ssh, SHELL_CHANNEL_ID,
                               (const byte *)usage, (word32)strlen(usage));
        goto cleanup;
    }

cleanup:
    ESP_LOGI(TAG, "SSH session closed [%s]", device_name[0] ? device_name : "-");
    if (device_write_cb) {
        ssh_release_device(device_name);
    }
    xSemaphoreTake(s_ssh_mutex, portMAX_DELAY);
    s_active_ssh = NULL;
    wolfSSH_free(ssh);
    xSemaphoreGive(s_ssh_mutex);

    close(client_fd);
    vTaskDelete(NULL);
}

/* ── SSH listener task ───────────────────────────────────────────────────── */

static void ssh_listen_task(void *arg)
{
    (void)arg;

    int listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        ESP_LOGE(TAG, "socket() failed: %d", errno);
        vTaskDelete(NULL);
        return;
    }

    /* Allow address reuse so we can restart quickly */
    int opt = 1;
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr = {
        .sin_family      = AF_INET,
        .sin_port        = htons(s_port),
        .sin_addr.s_addr = htonl(INADDR_ANY),
    };

    if (bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        ESP_LOGE(TAG, "bind() failed: %d", errno);
        close(listen_fd);
        vTaskDelete(NULL);
        return;
    }

    if (listen(listen_fd, 1) < 0) {
        ESP_LOGE(TAG, "listen() failed: %d", errno);
        close(listen_fd);
        vTaskDelete(NULL);
        return;
    }

    ESP_LOGI(TAG, "SSH server listening on port %u", s_port);
    {
        char tmp[64];
        snprintf(tmp, sizeof(tmp), "SSH server listening on port %u", s_port);
        ssh_log_info(tmp);
    }

    while (1) {
        struct sockaddr_in peer_addr;
        socklen_t peer_len = sizeof(peer_addr);
        int client_fd = accept(listen_fd,
                               (struct sockaddr *)&peer_addr, &peer_len);
        if (client_fd < 0) {
            ESP_LOGW(TAG, "accept() failed: %d", errno);
            vTaskDelay(100 / portTICK_PERIOD_MS);
            continue;
        }

        ESP_LOGI(TAG, "Incoming SSH connection from %s", inet_ntoa(peer_addr.sin_addr));
        {
            char tmp[80];
            snprintf(tmp, sizeof(tmp), "Incoming SSH from %s", inet_ntoa(peer_addr.sin_addr));
            ssh_log_info(tmp);
        }

        /* If a session is already active, check whether it is still alive.
         * If the previous session task crashed/was killed without going through
         * cleanup (e.g. panic, watchdog), s_active_ssh stays non-NULL and all
         * subsequent connections would be permanently rejected.
         * Guard the check with s_ssh_mutex to avoid a TOCTOU race. */
        xSemaphoreTake(s_ssh_mutex, portMAX_DELAY);
        if (s_active_ssh != NULL) {
            int old_fd = wolfSSH_get_fd(s_active_ssh);
            /* Probe whether the underlying socket is still open */
            int probe = fcntl(old_fd, F_GETFL);
            int dead = (probe < 0 && errno == EBADF);
            if (!dead) {
                /* Try a one-byte peek-recv to detect a remotely-closed socket */
                char peek;
                int r = recv(old_fd, &peek, 1, MSG_PEEK | MSG_DONTWAIT);
                dead = (r == 0) || (r < 0 && errno != EAGAIN && errno != EWOULDBLOCK);
            }
            if (dead) {
                ESP_LOGW(TAG, "Stale session detected (fd=%d dead) — force-clearing", old_fd);
                ssh_log_warn("Stale session detected — force-clearing s_active_ssh");
                wolfSSH_free((WOLFSSH *)s_active_ssh);
                s_active_ssh = NULL;
            }
        }
        int busy = (s_active_ssh != NULL);
        xSemaphoreGive(s_ssh_mutex);

        if (busy) {
            ESP_LOGW(TAG, "Already have an active session — rejecting");
            ssh_log_warn("Rejecting: session already active");
            close(client_fd);
            continue;
        }

        /* Log free heap before creating 48KB session task */
        {
            char tmp[80];
            snprintf(tmp, sizeof(tmp), "Free heap before session task: %u",
                     (unsigned)esp_get_free_heap_size());
            ssh_log_info(tmp);
        }

        /* Spawn a session handler task (stack in PSRAM — internal SRAM is
         * too scarce when the USB host driver is active). */
        BaseType_t ret = xTaskCreateWithCaps(
            ssh_session_task,
            "ssh_session",
            SSH_SESSION_STACK,
            (void *)(intptr_t)client_fd,
            5,
            NULL,
            MALLOC_CAP_SPIRAM
        );
        if (ret != pdPASS) {
            ESP_LOGE(TAG, "xTaskCreate(ssh_session) failed");
            ssh_log_warn("xTaskCreate(ssh_session) FAILED — not enough memory?");
            close(client_fd);
        }
    }
}

/* ── Public API ──────────────────────────────────────────────────────────── */

void ssh_bridge_cdc_rx(const uint8_t *data, size_t len)
{
    // char tmp[96];
    // snprintf(tmp, sizeof(tmp), "[DBG] cdc_rx len=%d head=%lu tail=%lu", (int)len, (unsigned long)s_ring_head, (unsigned long)s_ring_tail);
    // ssh_log_info(tmp);
    ring_write(data, len);
}

int ssh_bridge_init(uint16_t port,
                    const char *username, const char *password)
{
    s_port     = port;
    s_password = password;  /* pointer must remain valid for program lifetime */

    ESP_LOGI(TAG, "SSH bridge: user='%s'", username ? username : "(any)");
    /* Create synchronisation primitives */
    s_ring_mutex = xSemaphoreCreateMutex();
    s_ssh_mutex  = xSemaphoreCreateMutex();
    if (!s_ring_mutex || !s_ssh_mutex) {
        ESP_LOGE(TAG, "Mutex creation failed");
        return -1;
    }

    /* Allocate ring buffer from PSRAM (too large for internal SRAM) */
    s_ring = (uint8_t *)heap_caps_malloc(CDC_RING_SZ, MALLOC_CAP_SPIRAM);
    if (!s_ring) {
        ESP_LOGE(TAG, "PSRAM ring buffer alloc failed (%d bytes)", CDC_RING_SZ);
        return -1;
    }
    ESP_LOGI(TAG, "CDC ring buffer: %d KB (PSRAM)", CDC_RING_SZ / 1024);

    /* Route wolfSSL/wolfCrypt heap allocations through stdlib malloc so that
     * ESP-IDF's SPIRAM_USE_MALLOC fallback is active.  By default wolfSSL
     * uses pvPortMalloc() (FreeRTOS internal-SRAM only) when FREERTOS is
     * defined, which starves the SSH handshake on systems with limited
     * internal SRAM. */
    wolfSSL_SetAllocators(malloc, free, realloc);

    /* Initialise wolfSSH */
    if (wolfSSH_Init() != WS_SUCCESS) {
        ESP_LOGE(TAG, "wolfSSH_Init() failed");
        return -2;
    }

    /* Create wolfSSH server context */
    s_ctx = wolfSSH_CTX_new(WOLFSSH_ENDPOINT_SERVER, NULL);
    if (!s_ctx) {
        ESP_LOGE(TAG, "wolfSSH_CTX_new() failed");
        return -3;
    }

    wolfSSH_SetUserAuth(s_ctx, ws_user_auth);
    wolfSSH_CTX_SetBanner(s_ctx, "ESP32-S3 SSH-Serial-Bridge\r\n");

    /* Load embedded ECC-256 host key (much less heap than RSA 2048 during handshake) */
    int rc = wolfSSH_CTX_UsePrivateKey_buffer(
        s_ctx,
        (const byte *)ecc_key_der_256_ssh,
        (word32)sizeof_ecc_key_der_256_ssh,
        WOLFSSH_FORMAT_ASN1
    );
    if (rc != WS_SUCCESS) {
        ESP_LOGE(TAG, "UsePrivateKey_buffer(ECC-256) failed: %d", rc);
        wolfSSH_CTX_free(s_ctx);
        s_ctx = NULL;
        return -4;
    }

    ESP_LOGI(TAG, "wolfSSH context ready (ECC-256 host key loaded)");

    /* Start listener task (stack in PSRAM) */
    BaseType_t ret = xTaskCreateWithCaps(
        ssh_listen_task,
        "ssh_listen",
        SSH_LISTEN_STACK,
        NULL,
        4,
        NULL,
        MALLOC_CAP_SPIRAM
    );
    if (ret != pdPASS) {
        ESP_LOGE(TAG, "xTaskCreate(ssh_listen) failed");
        return -5;
    }

    return 0;  /* success */
}
