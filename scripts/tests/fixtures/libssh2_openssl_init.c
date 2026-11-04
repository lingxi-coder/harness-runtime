/* Count entry and exit while still calling the real compiled OpenSSL backend. */
#include <assert.h>
#include <stdio.h>
#include <libssh2.h>

static int init_calls;
static int exit_calls;
void test_real_openssl_crypto_init(void);
void test_real_openssl_crypto_exit(void);
void _libssh2_openssl_crypto_init(void)
{
    init_calls++;
    test_real_openssl_crypto_init();
}
void _libssh2_openssl_crypto_exit(void)
{
    exit_calls++;
    test_real_openssl_crypto_exit();
}
int main(void)
{
    /* An externally initialized engine stays caller-owned throughout this
     * reference cycle, even if a later reference requests normal init. */
    assert(libssh2_init(LIBSSH2_INIT_NO_CRYPTO) == 0);
    assert(libssh2_init(0) == 0);
    assert(init_calls == 0 && exit_calls == 0);
    libssh2_exit();
    assert(exit_calls == 0);
    libssh2_exit();
    assert(exit_calls == 0);

    /* The inverse order is owned by libssh2 despite a later NO_CRYPTO ref. */
    assert(libssh2_init(0) == 0);
    assert(libssh2_init(LIBSSH2_INIT_NO_CRYPTO) == 0);
    assert(init_calls == 1 && exit_calls == 0);
    libssh2_exit();
    assert(exit_calls == 0);
    libssh2_exit();
    assert(exit_calls == 1);
    libssh2_exit();
    assert(exit_calls == 1);

    assert(libssh2_init(0) == 0);
    assert(init_calls == 2);
    libssh2_exit();
    assert(exit_calls == 2);
    puts("real OpenSSL first-call ownership and mixed references passed");
    return 0;
}
