/* Real merge_driver.c/runtime.c/vector.c/alloc.c with injected allocation and
 * rwlock failures. Merge execution and vector search are substitutes. */
#include "git2_util.h"
#include "merge_driver.h"
#include "runtime.h"
#include "vector.h"

#include <errno.h>
#include <stdarg.h>

#include "git_initializer_allocator.h"

static int rw_live, rw_initializations, rw_frees, fail_rwlock;
static int insert_calls, fail_insert;

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

int audit_merge_rwlock_init(pthread_rwlock_t *lock, const pthread_rwlockattr_t *attr)
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

int audit_merge_rwlock_destroy(pthread_rwlock_t *lock)
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

int git__bsearch(void **items, size_t count, const void *key,
                 int (*cmp)(const void *, const void *), size_t *position)
{
    size_t i;
    for (i = 0; i < count && cmp(key, items[i]) > 0; i++) {}
    if (position)
        *position = i;
    return (i < count && cmp(key, items[i]) == 0) ? 0 : GIT_ENOTFOUND;
}

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

int audit_merge_vector_insert_sorted(git_vector *vector, void *element,
                                     int (*on_dup)(void **, void *))
{
    if (++insert_calls == fail_insert)
        return -1;
    return git_vector_insert_sorted(vector, element, on_dup);
}

int git_merge_file_from_index(git_merge_file_result *out, git_repository *repo,
                              const git_index_entry *ancestor, const git_index_entry *ours,
                              const git_index_entry *theirs, const git_merge_file_options *opts)
{
    (void)out; (void)repo; (void)ancestor; (void)ours; (void)theirs; (void)opts;
    return -1;
}

void git_merge_file_result_free(git_merge_file_result *result) { (void)result; }

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
    git_runtime_init_fn fns[] = { git_merge_driver_global_init };
    git_runtime_init_fn full[] = { fill_shutdown_registry, git_merge_driver_global_init };
    int result, observed_calls;

    CHECK(argc == 2);
    git__allocator = (git_allocator){ audit_malloc, audit_realloc, audit_free };
    if (!strcmp(argv[1], "lock"))
        fail_rwlock = 1;
    else if (!strncmp(argv[1], "insert", 6))
        fail_insert = atoi(argv[1] + 6);
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
        fail_insert = 0;
        CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) == 1);
        CHECK(git_runtime_shutdown() == 0);
    }
    CHECK(live_allocations == 0 && rw_live == 0 && rw_initializations == rw_frees);
    printf("allocations=%d\n", observed_calls);
    return 0;
}
