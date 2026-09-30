/* Vendored ref10 hashing and randomness use the configured crypto backends. */
#include "libssh2_priv.h"
#include "crypto_api.h"
#include <mbedtls/sha512.h>
#include <stdlib.h>

int crypto_hash_sha512(unsigned char *out, const unsigned char *in,
                       unsigned long long inlen)
{
    if(mbedtls_sha512(in, (size_t)inlen, out, 0) != 0)
        abort();
    return 0;
}

void libssh2_ed25519_randombytes(void *buf, size_t len)
{
    if(_libssh2_random((unsigned char *)buf, len) != 0)
        abort();
}
