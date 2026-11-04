/* Tests the exact extracted Windows socket initializer with a mocked WinSock
 * API and actual runtime.c; it is not a real Windows API or ABI test. */
#include "git_winsock_overrides.h"

#include <stdarg.h>

static int live, starts, cleanups, fail_start, wrong_version;

#define CHECK(condition) do { \
    if (!(condition)) { \
        fprintf(stderr, "%s:%d: %s (live=%d starts=%d cleanups=%d)\n", \
                __FILE__, __LINE__, #condition, live, starts, cleanups); \
        return 1; \
    } \
} while (0)

void git_error_set(int error_class, const char *format, ...)
{
    (void)error_class;
    (void)format;
}

int WSAStartup(WORD requested, WSADATA *data)
{
    if (requested != MAKEWORD(2, 2) || fail_start)
        return -1;
    starts++;
    live++;
    data->wVersion = wrong_version ? MAKEWORD(1, 1) : MAKEWORD(2, 2);
    return 0;
}

int WSACleanup(void)
{
    if (!live)
        return -1;
    live--;
    cleanups++;
    return 0;
}

int git_socket_stream_global_init(void);
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
    git_runtime_init_fn fns[] = { git_socket_stream_global_init };
    git_runtime_init_fn full[] = { fill_shutdown_registry, git_socket_stream_global_init };
    int repeats;

    CHECK(argc == 2);
    if (!strcmp(argv[1], "startup"))
        fail_start = 1;
    else if (!strcmp(argv[1], "version"))
        wrong_version = 1;
    else if (strcmp(argv[1], "register"))
        return 2;

    repeats = wrong_version ? 40 : 1;
    for (int i = 0; i < repeats; i++) {
        if (!strcmp(argv[1], "register"))
            CHECK(git_runtime_init(full, ARRAY_SIZE(full)) < 0);
        else
            CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) < 0);
        CHECK(git_runtime_init_count() == 0 && live == 0 && starts == cleanups);
    }
    wrong_version = fail_start = 0;
    CHECK(git_runtime_init(fns, ARRAY_SIZE(fns)) == 1 && live == 1);
    CHECK(git_runtime_shutdown() == 0);
    CHECK(live == 0 && starts == cleanups);
    return 0;
}
