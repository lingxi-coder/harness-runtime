/* includes.h — minimal shim for the vendored OpenSSH ref10 ed25519.c /
   crypto_api.h. Upstream this is an autoconf-generated header; the ref10 code
   only needs HAVE_STDINT_H so crypto_api.h pulls <stdint.h> for its
   int8_t..uint64_t typedefs. Nothing else from OpenSSH's includes.h is used. */
#ifndef LIBSSH2_ED25519_INCLUDES_H
#define LIBSSH2_ED25519_INCLUDES_H
#define HAVE_STDINT_H 1
#include <stdint.h>
#endif
