#!/usr/bin/env bash
# setup_components.sh
#
# Clone wolfssh and wolfssl into components/ at the exact commits used in
# this project, then overlay the ESP-IDF component files (CMakeLists.txt
# and wolfssl/include/user_settings.h).
#
# Usage:
#   cd esp32-s3/ssh-serial-bridge
#   bash setup_components.sh
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMP_DIR="${SCRIPT_DIR}/components"

WOLFSSH_REPO="https://github.com/wolfSSL/wolfssh.git"
WOLFSSH_COMMIT="157cb01f4b6061807c4f72c8c92d8c58ca889b6e"

WOLFSSL_REPO="https://github.com/wolfSSL/wolfssl.git"
WOLFSSL_COMMIT="b7e7e7555f0f949ec3b2045532a7a11c1b987ccb"

# ─── helpers ─────────────────────────────────────────────────────────────────

clone_or_update() {
    local name="$1"
    local repo="$2"
    local commit="$3"
    local dest="${COMP_DIR}/${name}"

    if [[ -d "${dest}/.git" ]]; then
        echo "[${name}] Repository already exists — fetching and checking out ${commit:0:10}..."
        git -C "${dest}" fetch --quiet origin
    else
        echo "[${name}] Cloning ${repo} ..."
        git clone --no-checkout --filter=blob:none "${repo}" "${dest}"
    fi

    git -C "${dest}" checkout --quiet "${commit}"
    echo "[${name}] Checked out ${commit:0:10}"
}

# ─── clone / update ───────────────────────────────────────────────────────────

mkdir -p "${COMP_DIR}"

clone_or_update "wolfssh" "${WOLFSSH_REPO}" "${WOLFSSH_COMMIT}"
clone_or_update "wolfssl" "${WOLFSSL_REPO}" "${WOLFSSL_COMMIT}"

# ─── overlay: components/wolfssh/CMakeLists.txt ──────────────────────────────

echo "[wolfssh] Writing CMakeLists.txt ..."
cat > "${COMP_DIR}/wolfssh/CMakeLists.txt" << 'EOF'
# components/wolfssh/CMakeLists.txt
#
# Compiles wolfSSH as an ESP-IDF component.
# Depends on the wolfssl component for wolfCrypt.
#
cmake_minimum_required(VERSION 3.16)

set(WOLFSSH_ROOT ${CMAKE_CURRENT_LIST_DIR})

# ── wolfSSH source files ──────────────────────────────────────────────────
set(WOLFSSH_SRCS
    "${WOLFSSH_ROOT}/src/agent.c"
    "${WOLFSSH_ROOT}/src/certman.c"
    "${WOLFSSH_ROOT}/src/internal.c"
    "${WOLFSSH_ROOT}/src/io.c"
    "${WOLFSSH_ROOT}/src/keygen.c"
    "${WOLFSSH_ROOT}/src/log.c"
    "${WOLFSSH_ROOT}/src/misc.c"
    "${WOLFSSH_ROOT}/src/port.c"
    "${WOLFSSH_ROOT}/src/ssh.c"
    "${WOLFSSH_ROOT}/src/wolfscp.c"
    "${WOLFSSH_ROOT}/src/wolfsftp.c"
    "${WOLFSSH_ROOT}/src/wolfterm.c"
)

# ── Component registration ────────────────────────────────────────────────
idf_component_register(
    SRCS         ${WOLFSSH_SRCS}
    INCLUDE_DIRS "."
    REQUIRES     wolfssl lwip freertos
)

# Make wolfssh headers accessible as <wolfssh/ssh.h>
target_include_directories(${COMPONENT_LIB} PUBLIC "${WOLFSSH_ROOT}")

# wolfSSL's settings.h uses #include "FreeRTOS.h" (non-PlatformIO path).
# In ESP-IDF 5.x FreeRTOS.h lives under freertos/ subdirectory, so add it directly.
target_include_directories(${COMPONENT_LIB} PRIVATE
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/include/freertos"
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/portable/xtensa/include/freertos"
)

# wolfssh also needs the wolfssl include directory for wolfCrypt headers
# (inherited transitively through wolfssl component, but be explicit)
target_compile_definitions(${COMPONENT_LIB}
    PUBLIC  WOLFSSL_USER_SETTINGS
            WOLFSSH_USER_SETTINGS
            # Reduce channel window/packet sizes for ESP32-S3 heap constraints.
            # Default (128KB window + 32KB packet) causes MALLOC failure → resource shortage.
            DEFAULT_WINDOW_SZ=16384
            DEFAULT_MAX_PACKET_SZ=4096
)

# Suppress harmless warnings in third-party code
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wno-unused-function
    -Wno-unused-variable
    -Wno-sign-compare
    -Wno-maybe-uninitialized
    -Wno-format
    -Wno-implicit-fallthrough
)
EOF

# ─── overlay: components/wolfssl/CMakeLists.txt ──────────────────────────────

echo "[wolfssl] Writing CMakeLists.txt ..."
cat > "${COMP_DIR}/wolfssl/CMakeLists.txt" << 'EOF'
# components/wolfssl/CMakeLists.txt
#
# Compiles wolfCrypt as an ESP-IDF component for use with wolfSSH.
# Only wolfCrypt is compiled; the TLS layer is not needed for SSH.
#
cmake_minimum_required(VERSION 3.16)

set(WOLFSSL_ROOT ${CMAKE_CURRENT_LIST_DIR})

# ── Source collection ──────────────────────────────────────────────────────
file(GLOB WOLFCRYPT_SRCS   "${WOLFSSL_ROOT}/wolfcrypt/src/*.c")

# wolfSSL FreeRTOS/ESP-IDF memory allocation wrappers (wc_pvPortMalloc etc.)
set(WOLFCRYPT_ESP_MEM_SRC
    "${WOLFSSL_ROOT}/wolfcrypt/src/port/Espressif/esp_sdk_mem_lib.c"
)

# Remove files that are #included by other .c files (would duplicate symbols)
# and the Espressif HW-acceleration port (incomplete / version-specific)
list(REMOVE_ITEM WOLFCRYPT_SRCS
    "${WOLFSSL_ROOT}/wolfcrypt/src/misc.c"
)

# ── Component registration ────────────────────────────────────────────────
idf_component_register(
    SRCS        ${WOLFCRYPT_SRCS} ${WOLFCRYPT_ESP_MEM_SRC}
    INCLUDE_DIRS "." "include"
    REQUIRES     lwip freertos
)

# Make WOLFSSL_ROOT accessible so code can do:
#   #include <wolfssl/wolfcrypt/settings.h>
#   #include <wolfssl/ssl.h>   (header-only, not compiled)
target_include_directories(${COMPONENT_LIB} PUBLIC "${WOLFSSL_ROOT}")

# wolfSSL's settings.h includes "FreeRTOS.h" (non-PlatformIO path).
# In ESP-IDF 5.x, FreeRTOS.h lives under FreeRTOS-Kernel/include/freertos/,
# so we add that sub-directory directly so bare #include "FreeRTOS.h" resolves.
target_include_directories(${COMPONENT_LIB} PRIVATE
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/include/freertos"
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/portable/xtensa/include/freertos"
)

# Propagate USER_SETTINGS flag to all consumers
target_compile_definitions(${COMPONENT_LIB} PUBLIC WOLFSSL_USER_SETTINGS)

# Suppress known harmless warnings in third-party code
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wno-unused-function
    -Wno-unused-variable
    -Wno-sign-compare
    -Wno-maybe-uninitialized
    -Wno-stringop-overflow
    -Wno-format
)
EOF

# ─── overlay: components/wolfssl/include/user_settings.h ─────────────────────

echo "[wolfssl] Writing include/user_settings.h ..."
mkdir -p "${COMP_DIR}/wolfssl/include"
cat > "${COMP_DIR}/wolfssl/include/user_settings.h" << 'EOF'
/* user_settings.h - wolfSSL/wolfCrypt configuration for ESP32-S3 SSH bridge
 *
 * Provides wolfCrypt crypto primitives required by wolfSSH.
 * No TLS layer needed — only wolfCrypt is compiled.
 */

#ifndef WOLF_USER_SETTINGS_H
#define WOLF_USER_SETTINGS_H

/* Pull in ESP-IDF sdkconfig for CONFIG_IDF_TARGET_* defines */
#include <sdkconfig.h>

/* ── wolfCrypt-only build (no TLS/SSL layer, no ssl.c) ───────────────────── */
/* Prevents wc_port.c from calling wolfSSL_EVP_init() which is only
 * available when ssl.c is compiled (sets WOLFSSL_EVP_INCLUDED). */
#define WOLFCRYPT_ONLY

/* ── ESP32 platform identity ─────────────────────────────────────────────── */
#undef  WOLFSSL_ESPIDF
#define WOLFSSL_ESPIDF

#undef  WOLFSSL_ESPWROOM32SE
#undef  WOLFSSL_ESP8266
#undef  WOLFSSL_ESP32
#define WOLFSSL_ESP32

/* ── No filesystem: use embedded key/cert buffers ────────────────────────── */
#define NO_FILESYSTEM

/* ── Crypto needed by wolfSSH ────────────────────────────────────────────── */

/* RSA for server host key */
/* (RSA is enabled by default; these options tune it) */
#define WC_RSA_PSS
#define WC_RSA_BLINDING
#define RSA_LOW_MEM
/* Disable ESP32 hardware RSA/MP acceleration — port files not compiled */
#undef  ESP32_USE_RSA_PRIMITIVE
#undef  WOLFSSL_ESP32_CRYPT_RSA_PRI
#define NO_ESP32_CRYPT
/* Disable all ESP32 hardware crypto (SHA/AES/MP) — software only */
#define NO_WOLFSSL_ESP32_CRYPT_HASH
#define NO_WOLFSSL_ESP32_CRYPT_AES
#define NO_WOLFSSL_ESP32_CRYPT_RSA_PRI

/* ECC for ECDSA host key / ECDH session key exchange */
#define HAVE_ECC
#define TFM_ECC256
#define ECC_SHAMIR
#define ECC_TIMING_RESISTANT

/* Curve25519/Ed25519 for modern SSH key types */
#define HAVE_CURVE25519
#define CURVE25519_SMALL
#define HAVE_ED25519        /* requires SHA-512 */

/* Hash algorithms */
#define WOLFSSL_SHA224
#define WOLFSSL_SHA384
#define WOLFSSL_SHA512      /* required for Ed25519 */

/* HMAC (message authentication) */
/* enabled by default */

/* Diffie-Hellman key exchange */
#define HAVE_DH
#define HAVE_FFDHE_2048
#define HAVE_DH_DEFAULT_PARAMS

/* Key Derivation */
#define HAVE_HKDF

/* AES-GCM (AEAD cipher) */
#define HAVE_AESGCM
#define GCM_TABLE_4BIT

/* ChaCha20-Poly1305 (AEAD cipher, preferred by modern SSH) */
#define HAVE_CHACHA
#define HAVE_POLY1305
#define HAVE_ONE_TIME_AUTH

/* Random number generation */
#define HAVE_HASHDRBG

/* ── Memory / stack optimisations ────────────────────────────────────────── */
#define USE_FAST_MATH
#define TFM_TIMING_RESISTANT
#define WOLFSSL_SMALL_STACK
#define BENCH_EMBEDDED

/* Embedded RSA/cert buffers (needed by certs_test.h) */
#define USE_CERT_BUFFERS_2048
#define USE_CERT_BUFFERS_256

/* ── Threading ───────────────────────────────────────────────────────────── */
#define HAVE_THREAD_LS
#define WC_NO_ASYNC_THREADING

/* ── Disable unused TLS / legacy features ────────────────────────────────── */
#define NO_OLD_TLS
#define NO_PSK
#define NO_DSA
#define NO_RC4
#define NO_DES3
#define NO_MD4

/* ── ESP32-S3 hardware acceleration ──────────────────────────────────────── */
#if defined(CONFIG_IDF_TARGET_ESP32S3)
    /* HW SHA and AES are available on S3; RSA HW math as well.
     * Leave all enabled (hardware fallback to SW is automatic). */

#elif defined(CONFIG_IDF_TARGET_ESP32)
    /* Standard ESP32 HW acceleration */
    #undef  ESP_RSA_MULM_BITS
    #define ESP_RSA_MULM_BITS 16

#else
    /* Unknown/unsupported target — disable HW acceleration */
    #define NO_ESP32_CRYPT
    #define NO_WOLFSSL_ESP32_CRYPT_HASH
    #define NO_WOLFSSL_ESP32_CRYPT_AES
    #define NO_WOLFSSL_ESP32_CRYPT_RSA_PRI
#endif

/* ── Miscellaneous ───────────────────────────────────────────────────────── */
#undef  WOLFSSL_ESPIDF_ERROR_PAUSE   /* no infinite loop on error */
#define WOLFSSL_PUBLIC_MP             /* expose mp_int for wolfSSH */

/* Key generation support (wolfSSH needs this for session keys) */
#define WOLFSSL_KEY_GEN
#define WOLFSSL_ASN_TEMPLATE

/* Optional: OpenSSL compatibility layer (used by some wolfSSH internals) */
#define OPENSSL_EXTRA

#endif /* WOLF_USER_SETTINGS_H */
EOF

# ─── static: xterm.js / css / addon-fit ──────────────────────────────────────

STATIC_DIR="${SCRIPT_DIR}/static"
mkdir -p "${STATIC_DIR}"

XTERM_VERSION="6.0.0"
ADDON_FIT_VERSION="0.11.0"

XTERM_JS_URL="https://unpkg.com/@xterm/xterm@${XTERM_VERSION}/lib/xterm.js"
XTERM_CSS_URL="https://unpkg.com/@xterm/xterm@${XTERM_VERSION}/css/xterm.css"
ADDON_FIT_URL="https://unpkg.com/@xterm/addon-fit@${ADDON_FIT_VERSION}/lib/addon-fit.js"

# Expected SHA-256 (verified against upstream)
XTERM_JS_SHA256="14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972"
XTERM_CSS_SHA256="854a7c0fb70e8b1a083c16797ab827299fb18744f5ad34f227b48337e33293c6"
ADDON_FIT_SHA256="ba3ea256ce0620a0992a197d6c9baea64823fc93d8da07a9e366ca9943c18527"

download_and_verify() {
    local label="$1"
    local url="$2"
    local dest="$3"
    local expected_sha="$4"

    echo "[static] Downloading ${label} ..."
    curl -fsSL --max-time 60 -o "${dest}" "${url}"

    local actual_sha
    actual_sha="$(sha256sum "${dest}" | awk '{print $1}')"
    if [[ "${actual_sha}" != "${expected_sha}" ]]; then
        echo "ERROR: SHA-256 mismatch for ${label}"
        echo "  expected : ${expected_sha}"
        echo "  actual   : ${actual_sha}"
        exit 1
    fi
    echo "[static] ${label} OK (sha256 verified)"
}

download_and_verify \
    "@xterm/xterm@${XTERM_VERSION} xterm.js" \
    "${XTERM_JS_URL}" \
    "${STATIC_DIR}/xterm.min.js" \
    "${XTERM_JS_SHA256}"

download_and_verify \
    "@xterm/xterm@${XTERM_VERSION} xterm.css" \
    "${XTERM_CSS_URL}" \
    "${STATIC_DIR}/xterm.min.css" \
    "${XTERM_CSS_SHA256}"

download_and_verify \
    "@xterm/addon-fit@${ADDON_FIT_VERSION} addon-fit.js" \
    "${ADDON_FIT_URL}" \
    "${STATIC_DIR}/xterm-addon-fit.min.js" \
    "${ADDON_FIT_SHA256}"

echo ""
echo "Done. Components are ready under ${COMP_DIR}/"
echo "  wolfssh : ${WOLFSSH_COMMIT:0:10}  (${WOLFSSH_REPO})"
echo "  wolfssl : ${WOLFSSL_COMMIT:0:10}  (${WOLFSSL_REPO})"
echo "Static files are ready under ${STATIC_DIR}/"
echo "  @xterm/xterm     : ${XTERM_VERSION}"
echo "  @xterm/addon-fit : ${ADDON_FIT_VERSION}"
