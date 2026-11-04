/* LingXi's shared Mbed TLS configuration for the library and its consumers. */
#ifndef LINGXI_MBEDTLS_CONFIG_H
#define LINGXI_MBEDTLS_CONFIG_H

/* The build script copies the unmodified upstream configuration here. */
#include "mbedtls/mbedtls_config_upstream.h"

/* Upstream self-test counters are shared and unsynchronized during ECP use. */
#undef MBEDTLS_SELF_TEST

/* TLS 1.3 and shared RNG contexts require Mbed TLS locking on Unix hosts. */
#if !defined(_WIN32) && (defined(__unix__) || defined(__APPLE__))
#define MBEDTLS_THREADING_C
#define MBEDTLS_THREADING_PTHREAD
#endif

#endif /* LINGXI_MBEDTLS_CONFIG_H */
