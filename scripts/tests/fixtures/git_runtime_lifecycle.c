/* Exercise the actual libgit2 runtime.c with real pthread locks and atomics.
 * Subsystem initializers below are controlled substitutes, not a full libgit2
 * integration or a test of the library's thread-local error implementation. */
#include "git2_util.h"
#include "runtime.h"

#include <stdarg.h>

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "check failed at %s:%d: %s\n", __FILE__, __LINE__, #condition); \
        return 1; \
    } \
} while (0)

/* Only the callback-capacity error path needs this dependency in runtime.c. */
void git_error_set(int error_class, const char *fmt, ...)
{
    (void)error_class;
    (void)fmt;
}

static int first_calls, second_calls, failure_code, cleanup_count;
static char cleanup_order[512];

static void cleanup_a(void) { cleanup_order[cleanup_count++] = 'A'; }
static void cleanup_b(void) { cleanup_order[cleanup_count++] = 'B'; }

static int init_a(void)
{
    first_calls++;
    return git_runtime_shutdown_register(cleanup_a);
}

static int init_b(void)
{
    second_calls++;
    if (git_runtime_shutdown_register(cleanup_b) < 0)
        return -1;
    return failure_code;
}

static git_runtime_init_fn initializers[] = {init_a, init_b};

static int failure_then_retry(void)
{
    int first, after_failure, retry, after_retry;

    failure_code = -7;
    first = git_runtime_init(initializers, ARRAY_SIZE(initializers));
    after_failure = git_runtime_init_count();
    failure_code = 0;
    retry = git_runtime_init(initializers, ARRAY_SIZE(initializers));
    after_retry = git_runtime_init_count();
    printf("failure=%d count_after_failure=%d retry=%d count_after_retry=%d "
           "init_calls=%d/%d cleanup_before_shutdown=%d\n",
           first, after_failure, retry, after_retry,
           first_calls, second_calls, cleanup_count);

    CHECK(first < 0 && after_failure == 0);
    CHECK(retry == 1 && after_retry == 1);
    CHECK(first_calls == 2 && second_calls == 2);
    CHECK(cleanup_count == 2 && cleanup_order[0] == 'B' && cleanup_order[1] == 'A');
    CHECK(git_runtime_shutdown() == 0);
    CHECK(cleanup_count == 4 && cleanup_order[2] == 'B' && cleanup_order[3] == 'A');
    CHECK(git_runtime_init_count() == 0);
    return 0;
}

static int repeated_and_positive_failure(void)
{
    int initial_cleanups = cleanup_count;

    /* Repeated errors must not leave callbacks behind or exhaust the registry. */
    failure_code = -1;
    for (int i = 0; i < 40; i++) {
        CHECK(git_runtime_init(initializers, ARRAY_SIZE(initializers)) < 0);
        CHECK(git_runtime_init_count() == 0);
    }
    CHECK(cleanup_count == initial_cleanups + 80);

    /* init_common treats every nonzero initializer result as failure. */
    failure_code = 1;
    CHECK(git_runtime_init(initializers, ARRAY_SIZE(initializers)) < 0);
    CHECK(git_runtime_init_count() == 0);
    CHECK(cleanup_count == initial_cleanups + 82);

    failure_code = 0;
    CHECK(git_runtime_init(initializers, ARRAY_SIZE(initializers)) == 1);
    CHECK(git_runtime_init(initializers, ARRAY_SIZE(initializers)) == 2);
    CHECK(git_runtime_shutdown() == 1);
    CHECK(cleanup_count == initial_cleanups + 82);
    CHECK(git_runtime_shutdown() == 0);
    CHECK(cleanup_count == initial_cleanups + 84);

    /* A shutdown without a successful init must not poison the next init. */
    CHECK(git_runtime_shutdown() < 0);
    CHECK(git_runtime_init_count() == 0);
    CHECK(git_runtime_init(initializers, ARRAY_SIZE(initializers)) == 1);
    CHECK(git_runtime_shutdown() == 0);
    return 0;
}

enum { WAITERS = 8 };
static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t condition = PTHREAD_COND_INITIALIZER;
static int entered, release_failure, waiting, race_inits, race_cleanups;
static int race_result[WAITERS + 1];

static void race_cleanup(void) { race_cleanups++; }

static int race_initializer(void)
{
    race_inits++;
    if (git_runtime_shutdown_register(race_cleanup) < 0)
        return -1;
    if (race_inits == 1) {
        pthread_mutex_lock(&gate);
        entered = 1;
        pthread_cond_broadcast(&condition);
        while (!release_failure)
            pthread_cond_wait(&condition, &gate);
        pthread_mutex_unlock(&gate);
        return -1;
    }
    return 0;
}

static git_runtime_init_fn race_initializers[] = {race_initializer};

static void *race_worker(void *arg)
{
    int index = *(int *)arg;
    if (index) {
        pthread_mutex_lock(&gate);
        waiting++;
        pthread_cond_broadcast(&condition);
        pthread_mutex_unlock(&gate);
    }
    race_result[index] = git_runtime_init(race_initializers, ARRAY_SIZE(race_initializers));
    return NULL;
}

static int concurrent_failure_and_retry(void)
{
    pthread_t threads[WAITERS + 1];
    int indices[WAITERS + 1], seen[WAITERS + 1] = {0};

    indices[0] = 0;
    CHECK(pthread_create(&threads[0], NULL, race_worker, &indices[0]) == 0);
    pthread_mutex_lock(&gate);
    while (!entered)
        pthread_cond_wait(&condition, &gate);
    pthread_mutex_unlock(&gate);

    for (int i = 1; i <= WAITERS; i++) {
        indices[i] = i;
        CHECK(pthread_create(&threads[i], NULL, race_worker, &indices[i]) == 0);
    }
    pthread_mutex_lock(&gate);
    while (waiting != WAITERS)
        pthread_cond_wait(&condition, &gate);
    release_failure = 1;
    pthread_cond_broadcast(&condition);
    pthread_mutex_unlock(&gate);

    for (int i = 0; i <= WAITERS; i++)
        CHECK(pthread_join(threads[i], NULL) == 0);
    CHECK(race_result[0] < 0 && race_inits == 2 && race_cleanups == 1);
    for (int i = 1; i <= WAITERS; i++) {
        CHECK(race_result[i] >= 1 && race_result[i] <= WAITERS);
        seen[race_result[i]]++;
    }
    for (int i = 1; i <= WAITERS; i++)
        CHECK(seen[i] == 1);
    CHECK(git_runtime_init_count() == WAITERS);
    for (int i = WAITERS - 1; i >= 0; i--)
        CHECK(git_runtime_shutdown() == i);
    CHECK(race_cleanups == 2 && git_runtime_init_count() == 0);
    puts("concurrent failure and eight serialized retry callers: OK");
    return 0;
}

int main(void)
{
    CHECK(git_runtime_init_count() == 0);
    CHECK(failure_then_retry() == 0);
    CHECK(repeated_and_positive_failure() == 0);
    CHECK(concurrent_failure_and_retry() == 0);
    puts("runtime lifecycle checks passed");
    return 0;
}
