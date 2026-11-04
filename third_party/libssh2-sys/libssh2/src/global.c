/* Copyright (C) Lars Nordin <Lars.Nordin@SDlabs.se>
 * Copyright (C) Simon Josefsson <simon@josefsson.org>
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms,
 * with or without modification, are permitted provided
 * that the following conditions are met:
 *
 *   Redistributions of source code must retain the above
 *   copyright notice, this list of conditions and the
 *   following disclaimer.
 *
 *   Redistributions in binary form must reproduce the above
 *   copyright notice, this list of conditions and the following
 *   disclaimer in the documentation and/or other materials
 *   provided with the distribution.
 *
 *   Neither the name of the copyright holder nor the names
 *   of any other contributors may be used to endorse or
 *   promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND
 * CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
 * INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
 * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR
 * CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
 * SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
 * BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
 * INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
 * WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
 * NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
 * USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY
 * OF SUCH DAMAGE.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 */

#include "libssh2_priv.h"

static int _libssh2_initialized = 0;
/* Ownership is decided by the first successful initialization. Later calls
 * add references but cannot transfer ownership of the active crypto state. */
static int _libssh2_crypto_owned = 0;

LIBSSH2_API int
libssh2_init(int flags)
{
    if(_libssh2_initialized == 0) {
        int init_crypto = !(flags & LIBSSH2_INIT_NO_CRYPTO);

#ifdef LIBSSH2_MBEDTLS
        /* The entropy and DRBG contexts are private to this backend. Even
         * NO_CRYPTO callers cannot initialize those contexts themselves. */
        init_crypto = 1;
#endif
        if(init_crypto) {
            int rc = libssh2_crypto_init();
            if(rc)
                return rc;
            _libssh2_crypto_owned = 1;
        }
    }

    _libssh2_initialized++;

    return 0;
}

LIBSSH2_API void
libssh2_exit(void)
{
    if(_libssh2_initialized == 0)
        return;

    _libssh2_initialized--;

    if(_libssh2_initialized == 0) {
        if(_libssh2_crypto_owned)
            libssh2_crypto_exit();
        _libssh2_crypto_owned = 0;
    }

    return;
}

int
_libssh2_init_if_needed(void)
{
    if(_libssh2_initialized == 0)
        return libssh2_init(0);
    return 0;
}
