/* Real sysdir.c/runtime.c/str.c/alloc.c with controlled directory inputs and
 * a counting allocator. The passwd source is a deterministic substitute. */
#include "git2_util.h"
#include "runtime.h"
#include "str.h"
#include "sysdir.h"

#include <errno.h>
#include <pwd.h>
#include <stdarg.h>
#include <unistd.h>

#include "git_initializer_allocator.h"

static int use_passwd;

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "%s:%d: %s (live=%d calls=%d failure=%d passwd=%d)\n", \
                __FILE__, __LINE__, #condition, live_allocations, allocation_calls, \
                fail_allocation, use_passwd); \
        return 1; \
    } \
} while (0)

void git_error_set(int error_class, const char *format, ...)
{
    (void)error_class;
    (void)format;
}
void git_error_set_oom(void) {}
void git_error_clear(void) {}

int git__getenv(git_str *out, const char *name)
{
    if (!strcmp(name, "HOME"))
        return git_str_sets(out, "/audit/home");
    if (!strcmp(name, "XDG_CONFIG_HOME"))
        return git_str_sets(out, "/audit/config");
    return GIT_ENOTFOUND;
}

char *audit_sysdir_getenv(const char *name) { (void)name; return NULL; }
uid_t audit_sysdir_getuid(void) { return 100; }
uid_t audit_sysdir_geteuid(void) { return use_passwd ? 200 : 100; }
long audit_sysdir_sysconf(int name) { (void)name; return 1024; }

int audit_sysdir_getpwuid_r(uid_t uid, struct passwd *pwd, char *buffer,
                             size_t size, struct passwd **result)
{
    (void)uid;
    if (!buffer)
        return ENOMEM;
    if (size < 2048)
        return ERANGE;
    pwd->pw_dir = "/audit/passwd";
    *result = pwd;
    return 0;
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
    git_runtime_init_fn fns[] = { git_sysdir_global_init };
    git_runtime_init_fn full[] = { fill_shutdown_registry, git_sysdir_global_init };
    int result, observed_calls, success, registration;

    CHECK(argc == 3);
    use_passwd = !strcmp(argv[1], "passwd");
    success = !strcmp(argv[2], "success");
    registration = !strcmp(argv[2], "register");
    if (!success && !registration)
        fail_allocation = atoi(argv[2]);
    git__allocator = (git_allocator){ audit_malloc, audit_realloc, audit_free };

    result = registration ? git_runtime_init(full, ARRAY_SIZE(full)) :
                            git_runtime_init(fns, ARRAY_SIZE(fns));
    observed_calls = allocation_calls;
    if (success) {
        CHECK(result == 1 && live_allocations > 0);
        CHECK(git_runtime_shutdown() == 0);
    } else {
        CHECK(result < 0 && git_runtime_init_count() == 0);
        if (fail_allocation)
            CHECK(observed_calls >= fail_allocation);
        CHECK(live_allocations == 0);
        fail_allocation = 0;
        CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) == 1);
        CHECK(git_runtime_shutdown() == 0);
    }
    CHECK(live_allocations == 0);
    printf("allocations=%d\n", observed_calls);
    return 0;
}
