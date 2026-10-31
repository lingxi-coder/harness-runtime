/* Native positive and negative controls; every probe child exits immediately. */
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int probe(const char *mode, int expect_denial) {
    int error = 0;
    int result = -1;
    pid_t before = getpgrp();
    if (strcmp(mode, "setsid") == 0) {
        result = setsid();
        error = result < 0 ? errno : 0;
    } else if (strcmp(mode, "setpgid") == 0) {
        result = setpgid(0, 0);
        error = result < 0 ? errno : 0;
    } else if (strcmp(mode, "raw-setsid") == 0) {
        result = syscall(SYS_setsid);
        error = result < 0 ? errno : 0;
    } else if (strcmp(mode, "raw-setpgid") == 0) {
        result = syscall(SYS_setpgid, 0, 0);
        error = result < 0 ? errno : 0;
    } else {
        posix_spawnattr_t attr;
        error = posix_spawnattr_init(&attr);
        if (error != 0) return 20;
        short flags = 0;
        if (strcmp(mode, "spawn-session") == 0) flags = POSIX_SPAWN_SETSID;
        else if (strcmp(mode, "spawn-group") == 0) flags = POSIX_SPAWN_SETPGROUP;
        else if (strcmp(mode, "spawn") != 0) return 21;
        error = posix_spawnattr_setflags(&attr, flags);
        if (error != 0) return 22;
        if (flags == POSIX_SPAWN_SETPGROUP) {
            error = posix_spawnattr_setpgroup(&attr, 0);
            if (error != 0) return 23;
        }
        pid_t child = -1;
        char *const args[] = {"/usr/bin/true", NULL};
        error = posix_spawn(&child, args[0], NULL, &attr, args, environ);
        posix_spawnattr_destroy(&attr);
        result = error == 0 ? 0 : -1;
        if (error == 0) {
            int status;
            while (waitpid(child, &status, 0) < 0) {
                if (errno != EINTR) return 24;
            }
            if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 25;
        }
    }
    int denied = result < 0 && (error == EPERM || error == EACCES);
    int unchanged = before == getpgrp();
    printf("%s result=%d errno=%d group_unchanged=%d\n", mode, result, error, unchanged);
    fflush(stdout);
    if (expect_denial) return denied && unchanged ? 0 : 30;
    return result >= 0 && error == 0 ? 0 : 31;
}

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    /* The tested process must not already be a group/session leader. */
    pid_t child = fork();
    if (child < 0) return 3;
    if (child == 0) _exit(probe(argv[1], strcmp(argv[2], "deny") == 0));
    int status;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) return 4;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : 5;
}
