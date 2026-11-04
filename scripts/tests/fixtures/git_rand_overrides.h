#include <stddef.h>

/* Some POSIX feature profiles do not declare getentropy in unistd.h. */
int audit_rand_getentropy(void *buffer, size_t length);
