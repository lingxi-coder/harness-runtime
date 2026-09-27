/* ed25519_glue.c — satisfy the vendored ref10's one external dep using mbedTLS.
   crypto_hash_sha512: ref10's required hash, mapped to mbedTLS SHA-512.
   (randombytes is a macro→arc4random_buf in crypto_api.h — not defined here;
    ed25519 keygen is never invoked in this build.) */
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
