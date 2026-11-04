/* Real filter.c, runtime.c, vector.c, str.c, and alloc.c with fault-injected
 * allocations/locks and small substitutes for the two filter constructors. */
#include "git2_util.h"
#include "filter.h"
#include "runtime.h"

#include <errno.h>
#include <stdarg.h>

#include "git_initializer_allocator.h"

static int rw_live, rw_initializations, rw_frees, fail_rwlock;

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "%s:%d: %s (allocations=%d calls=%d rw_live=%d rw_init=%d rw_free=%d)\n", \
                __FILE__, __LINE__, #condition, live_allocations, allocation_calls, \
                rw_live, rw_initializations, rw_frees); \
        return 1; \
    } \
} while (0)

void git_error_set(int error_class, const char *format, ...)
{
    (void)error_class;
    (void)format;
}

void git_error_set_oom(void) {}

int audit_filter_rwlock_init(pthread_rwlock_t *lock, const pthread_rwlockattr_t *attr)
{
    int result;
    if (fail_rwlock)
        return EAGAIN;
    if (rw_live)
        return EBUSY;
    result = pthread_rwlock_init(lock, attr);
    if (!result) {
        rw_live++;
        rw_initializations++;
    }
    return result;
}

int audit_filter_rwlock_destroy(pthread_rwlock_t *lock)
{
    int result;
    if (!rw_live)
        return EINVAL;
    result = pthread_rwlock_destroy(lock);
    if (!result) {
        rw_live--;
        rw_frees++;
    }
    return result;
}

/* Definitions used by the real filter attribute parser. */
const char *git_attr__true = "true";
const char *git_attr__false = "false";
const char *git_attr__unset = "unset";

static git_filter *new_filter(const char *attributes)
{
    git_filter *filter = git__calloc(1, sizeof(*filter));
    if (filter) {
        filter->attributes = attributes;
        filter->shutdown = git_filter_free;
    }
    return filter;
}

git_filter *git_crlf_filter_new(void) { return new_filter("crlf eol text"); }
git_filter *git_ident_filter_new(void) { return new_filter("+ident"); }

/* The real vector sorts elements through this helper. */
void git__tsort(void **items, size_t count, int (*cmp)(const void *, const void *))
{
    for (size_t i = 0; i < count; i++)
        for (size_t j = i + 1; j < count; j++)
            if (cmp(items[i], items[j]) > 0) {
                void *item = items[i];
                items[i] = items[j];
                items[j] = item;
            }
}

static void no_op(void) {}
static int fill_shutdown_registry(void)
{
    for (int i = 0; i < 32; i++)
        if (git_runtime_shutdown_register(no_op) != 0)
            return -1;
    return 0;
}

int main(int argc, char **argv)
{
    git_runtime_init_fn fns[] = { git_filter_global_init };
    git_runtime_init_fn full[] = { fill_shutdown_registry, git_filter_global_init };
    int result, observed_calls;

    CHECK(argc == 2);
    git__allocator = (git_allocator){ audit_malloc, audit_realloc, audit_free };
    if (!strcmp(argv[1], "lock"))
        fail_rwlock = 1;
    else if (strcmp(argv[1], "success") && strcmp(argv[1], "register"))
        fail_allocation = atoi(argv[1]);

    result = !strcmp(argv[1], "register") ?
        git_runtime_init(full, ARRAY_SIZE(full)) : git_runtime_init(fns, ARRAY_SIZE(fns));
    observed_calls = allocation_calls;
    if (!strcmp(argv[1], "success")) {
        CHECK(result == 1 && live_allocations > 0 && rw_live == 1);
        CHECK(git_runtime_shutdown() == 0);
    } else {
        CHECK(result < 0 && git_runtime_init_count() == 0);
        if (fail_allocation)
            CHECK(observed_calls >= fail_allocation);
        CHECK(live_allocations == 0 && rw_live == 0 && rw_initializations == rw_frees);
        fail_allocation = 0;
        fail_rwlock = 0;
        CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) == 1);
        CHECK(git_runtime_shutdown() == 0);
    }
    CHECK(live_allocations == 0 && rw_live == 0 && rw_initializations == rw_frees);
    printf("allocations=%d\n", observed_calls);
    return 0;
}
