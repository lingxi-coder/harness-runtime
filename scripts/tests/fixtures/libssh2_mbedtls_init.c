/* Actual libssh2 sources linked to the pinned Mbed TLS libraries.  Entropy
 * failure is injected at the OS boundary; no crypto operations are replaced. */
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libssh2.h>
#include <mbedtls/ctr_drbg.h>
#include <mbedtls/entropy.h>
#include <mbedtls/platform.h>
#include <psa/crypto.h>
#include <psa/crypto_extra.h>

#if defined(__linux__) && defined(__GLIBC__)
#include <stdarg.h>
#include <sys/syscall.h>
static int fail_os_entropy;
long __real_syscall(long number, ...);
long __wrap_syscall(long number, ...)
{
    va_list args;
    void *buffer;
    size_t size;
    unsigned int flags;
    assert(number == SYS_getrandom);
    va_start(args, number);
    buffer = va_arg(args, void *);
    size = va_arg(args, size_t);
    flags = va_arg(args, unsigned int);
    va_end(args);
    if(fail_os_entropy) {
        errno = EIO;
        return -1;
    }
    return __real_syscall(number, buffer, size, flags);
}
static void set_entropy_failure(int fail) { fail_os_entropy = fail; }
#elif defined(__APPLE__) && defined(MBEDTLS_PLATFORM_HAVE_DEV_RANDOM)
static const char *saved_random;
static void set_entropy_failure(int fail)
{
    if(fail) {
        saved_random = mbedtls_platform_dev_random;
        mbedtls_platform_dev_random = "/nonexistent/lingxi-libssh2-entropy-test";
    }
    else
        mbedtls_platform_dev_random = saved_random;
}
#else
#error This test needs a supported real OS entropy failure injection mechanism
#endif

/* A private existing backend function, used only by this component test. */
extern int _libssh2_mbedtls_random(unsigned char *buffer, size_t length);

static int allocations;
static int frees;
static void *tracked_alloc(size_t count, void **abstract)
{
    (void)abstract;
    allocations++;
    return malloc(count);
}
static void tracked_free(void *ptr, void **abstract)
{
    (void)abstract;
    frees++;
    free(ptr);
}
static void *tracked_realloc(void *ptr, size_t count, void **abstract)
{
    (void)abstract;
    return realloc(ptr, count);
}
static void test_real_entropy_control(int expect_failure)
{
    mbedtls_entropy_context entropy;
    mbedtls_ctr_drbg_context drbg;
    mbedtls_entropy_init(&entropy);
    mbedtls_ctr_drbg_init(&drbg);
    int rc = mbedtls_ctr_drbg_seed(&drbg, mbedtls_entropy_func, &entropy,
                                   NULL, 0);
    assert((rc != 0) == expect_failure);
    mbedtls_ctr_drbg_free(&drbg);
    mbedtls_entropy_free(&entropy);
}
static void test_random(void)
{
    unsigned char bytes[32];
    assert(_libssh2_mbedtls_random(bytes, sizeof(bytes)) == 0);
}
static void test_references(int first, int second)
{
    LIBSSH2_SESSION *session;
    assert(libssh2_init(first) == 0);
    assert(libssh2_init(second) == 0);
    test_random();
    set_entropy_failure(1);
    libssh2_exit();
    /* The second reference must keep the existing generator alive without
     * trying to seed it again, even though OS entropy is now unavailable. */
    session = libssh2_session_init();
    assert(session != NULL);
    test_random();
    assert(libssh2_session_free(session) == 0);
    libssh2_exit();
    /* A failed new initialization proves the last exit released its owner. */
    assert(libssh2_init(first) == LIBSSH2_ERROR_RANDGEN);
    set_entropy_failure(0);
}
int main(void)
{
    unsigned char bytes[32];
    LIBSSH2_SESSION *session;
    mbedtls_psa_crypto_free();
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_ERROR_BAD_STATE);

    test_real_entropy_control(0);
    set_entropy_failure(1);
    test_real_entropy_control(1);
    assert(libssh2_init(0) == LIBSSH2_ERROR_RANDGEN);
    assert(libssh2_init(LIBSSH2_INIT_NO_CRYPTO) == LIBSSH2_ERROR_RANDGEN);
    /* An exit after either failed init must not consume a later reference. */
    libssh2_exit();
    allocations = frees = 0;
    session = libssh2_session_init_ex(tracked_alloc, tracked_free,
                                      tracked_realloc, NULL);
    assert(session == NULL && allocations == 1 && frees == 1);
    set_entropy_failure(0);
    test_real_entropy_control(0);

    /* NO_CRYPTO still supplies a usable private generator, while never
     * initializing a caller-owned PSA context. */
    assert(libssh2_init(LIBSSH2_INIT_NO_CRYPTO) == 0);
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_ERROR_BAD_STATE);
    test_random();
    libssh2_exit();
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_ERROR_BAD_STATE);

    assert(psa_crypto_init() == PSA_SUCCESS);
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_SUCCESS);
    assert(libssh2_init(LIBSSH2_INIT_NO_CRYPTO) == 0);
    test_random();
    libssh2_exit();
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_SUCCESS);

    test_references(0, LIBSSH2_INIT_NO_CRYPTO);
    test_references(LIBSSH2_INIT_NO_CRYPTO, 0);
    for(int i = 0; i < 8; ++i) {
        assert(libssh2_init(0) == 0);
        test_random();
        libssh2_exit();
    }
    assert(psa_generate_random(bytes, sizeof(bytes)) == PSA_SUCCESS);
    mbedtls_psa_crypto_free();
    puts("libssh2 real entropy failure, cleanup, retry, references and PSA ownership passed");
    return 0;
}
