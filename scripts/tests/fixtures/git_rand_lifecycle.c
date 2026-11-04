/* Tests the real libgit2 rand.c and runtime.c with fault-injected entropy,
 * shutdown registration and mutex operations. This is a component test. */
#include "git2_util.h"
#include "rand.h"
#include "runtime.h"

#include <errno.h>
#include <stdarg.h>
#include <sys/time.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "%s:%d: %s (live=%d init=%d free=%d duplicate=%d locks=%d)\n", \
                __FILE__, __LINE__, #condition, live, inits, frees, duplicates, locks); \
        return 1; \
    } \
} while (0)

static int live, inits, frees, duplicates, locks, fail_mutex, seed_mode;

void git_error_set(int error_class, const char *format, ...)
{
    (void)error_class;
    (void)format;
}

/* These names replace pthread calls only in the separately compiled rand.c. */
int audit_rand_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr)
{
    int result;

    if (fail_mutex)
        return EAGAIN; /* POSIX returns a positive error number. */
    if (live) {
        duplicates++;
        return EBUSY;
    }
    result = pthread_mutex_init(mutex, attr);
    if (!result) {
        live++;
        inits++;
    }
    return result;
}

int audit_rand_mutex_destroy(pthread_mutex_t *mutex)
{
    int result;
    if (!live)
        return EINVAL;
    result = pthread_mutex_destroy(mutex);
    if (!result) {
        live--;
        frees++;
    }
    return result;
}

int audit_rand_mutex_lock(pthread_mutex_t *mutex)
{
    locks++;
    return live ? pthread_mutex_lock(mutex) : EINVAL;
}

int audit_rand_mutex_unlock(pthread_mutex_t *mutex)
{
    return live ? pthread_mutex_unlock(mutex) : EINVAL;
}

int audit_rand_getentropy(void *buffer, size_t length)
{
    uint64_t seed = (seed_mode == 1) ? 0 : UINT64_C(0x0123456789abcdef);
    if (seed_mode == 2)
        return -1;
    if (length != sizeof(seed))
        return -1;
    memcpy(buffer, &seed, sizeof(seed));
    return 0;
}

int audit_rand_gettimeofday(struct timeval *tv, void *zone)
{
    (void)tv;
    (void)zone;
    return -1;
}

static void no_op(void) {}

static int fill_shutdown_registry(void)
{
    for (int i = 0; i < 32; i++)
        if (git_runtime_shutdown_register(no_op) != 0)
            return -1;
    return 0;
}

static int retry(void)
{
    git_runtime_init_fn fns[] = { git_rand_global_init };

    for (int i = 0; i < 40; i++) {
        seed_mode = 1;
        CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) < 0);
        CHECK(git_runtime_init_count() == 0 && live == 0);
    }
    seed_mode = 0;
    CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) == 1);
    CHECK(live == 1 && duplicates == 0);
    (void)git_rand_next();
    CHECK(git_runtime_shutdown() == 0);
    CHECK(live == 0 && inits == frees);
    return 0;
}

int main(int argc, char **argv)
{
    git_runtime_init_fn fns[] = { git_rand_global_init };
    git_runtime_init_fn full[] = { fill_shutdown_registry, git_rand_global_init };

    CHECK(argc == 2);
    if (!strcmp(argv[1], "retry"))
        return retry();
    if (!strcmp(argv[1], "seed-zero"))
        seed_mode = 1;
    else if (!strcmp(argv[1], "seed-error"))
        seed_mode = 2;
    else if (!strcmp(argv[1], "mutex-error"))
        fail_mutex = 1;
    else if (!strcmp(argv[1], "register-full")) {
        CHECK(git_runtime_init(full, ARRAY_SIZE(full)) < 0);
        CHECK(git_runtime_init_count() == 0 && live == 0 && inits == frees);
        return 0;
    } else {
        return 2;
    }

    CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) < 0);
    CHECK(git_runtime_init_count() == 0 && live == 0 && inits == frees);
    if (fail_mutex)
        CHECK(inits == 0 && frees == 0 && locks == 0);
    return 0;
}
