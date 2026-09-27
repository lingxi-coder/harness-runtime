/*
 * Kernel-6.14 securebits ABI constants (uapi/linux/securebits.h values).
 *
 * Why this shim exists: minijail's system.c `_Static_assert`s
 * `SECURE_ALL_BITS == 0x555` on __ANDROID__ (the kernel-6.14 bit set, which
 * added SECBIT_EXEC_RESTRICT_FILE and SECBIT_EXEC_DENY_INTERACTIVE), but
 * both NDK r27's sysroot and libcap-2.69's vendored uapi headers predate
 * 6.14 and stop at SECURE_ALL_BITS == 0x55. This directory is placed FIRST
 * on the include path by build.rs so libminijail compiles against the bit
 * set it expects; system.c falls back at runtime (EPERM -> retry without
 * the 6.14 bits) on older kernels.
 *
 * These are kernel ABI constants, not copied expression; layout follows the
 * uapi header so the macro names resolve identically.
 */
#ifndef _LINUX_SECUREBITS_H
#define _LINUX_SECUREBITS_H 1

#define issecure_mask(X) (1 << (X))

#define SECUREBITS_DEFAULT 0x00000000

/* When set UID 0 has no special privileges. */
#define SECURE_NOROOT 0
#define SECURE_NOROOT_LOCKED 1 /* make bit-0 immutable */
#define SECBIT_NOROOT (issecure_mask(SECURE_NOROOT))
#define SECBIT_NOROOT_LOCKED (issecure_mask(SECURE_NOROOT_LOCKED))

/* Setuid apps run with capabilities based on file caps only. */
#define SECURE_NO_SETUID_FIXUP 2
#define SECURE_NO_SETUID_FIXUP_LOCKED 3 /* make bit-2 immutable */
#define SECBIT_NO_SETUID_FIXUP (issecure_mask(SECURE_NO_SETUID_FIXUP))
#define SECBIT_NO_SETUID_FIXUP_LOCKED                                          \
	(issecure_mask(SECURE_NO_SETUID_FIXUP_LOCKED))

/* Keep permitted capabilities across uid 0->nonzero transition. */
#define SECURE_KEEP_CAPS 4
#define SECURE_KEEP_CAPS_LOCKED 5 /* make bit-4 immutable */
#define SECBIT_KEEP_CAPS (issecure_mask(SECURE_KEEP_CAPS))
#define SECBIT_KEEP_CAPS_LOCKED (issecure_mask(SECURE_KEEP_CAPS_LOCKED))

/* Disallow raising ambient capabilities. */
#define SECURE_NO_CAP_AMBIENT_RAISE 6
#define SECURE_NO_CAP_AMBIENT_RAISE_LOCKED 7 /* make bit-6 immutable */
#define SECBIT_NO_CAP_AMBIENT_RAISE (issecure_mask(SECURE_NO_CAP_AMBIENT_RAISE))
#define SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED                                     \
	(issecure_mask(SECURE_NO_CAP_AMBIENT_RAISE_LOCKED))

/* Restrict file execution to executable mappings (kernel 6.14). */
#define SECURE_EXEC_RESTRICT_FILE 8
#define SECURE_EXEC_RESTRICT_FILE_LOCKED 9 /* make bit-8 immutable */
#define SECBIT_EXEC_RESTRICT_FILE (issecure_mask(SECURE_EXEC_RESTRICT_FILE))
#define SECBIT_EXEC_RESTRICT_FILE_LOCKED                                       \
	(issecure_mask(SECURE_EXEC_RESTRICT_FILE_LOCKED))

/* Deny interactive (tty-input) execution (kernel 6.14). */
#define SECURE_EXEC_DENY_INTERACTIVE 10
#define SECURE_EXEC_DENY_INTERACTIVE_LOCKED 11 /* make bit-10 immutable */
#define SECBIT_EXEC_DENY_INTERACTIVE                                           \
	(issecure_mask(SECURE_EXEC_DENY_INTERACTIVE))
#define SECBIT_EXEC_DENY_INTERACTIVE_LOCKED                                    \
	(issecure_mask(SECURE_EXEC_DENY_INTERACTIVE_LOCKED))

#define SECURE_ALL_BITS                                                        \
	(issecure_mask(SECURE_NOROOT) | issecure_mask(SECURE_NO_SETUID_FIXUP) |\
	 issecure_mask(SECURE_KEEP_CAPS) |                                     \
	 issecure_mask(SECURE_NO_CAP_AMBIENT_RAISE) |                          \
	 issecure_mask(SECURE_EXEC_RESTRICT_FILE) |                            \
	 issecure_mask(SECURE_EXEC_DENY_INTERACTIVE))
#define SECURE_ALL_LOCKS (SECURE_ALL_BITS << 1)

#endif /* _LINUX_SECUREBITS_H */
