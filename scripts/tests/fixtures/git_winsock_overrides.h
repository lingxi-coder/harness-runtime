#include "git2_util.h"
#include "runtime.h"

/* Only the WinSock version and startup/cleanup contract are modeled. */
typedef uint16_t WORD;
typedef struct { WORD wVersion; } WSADATA;
#define MAKEWORD(low, high) ((WORD)((low) | ((high) << 8)))
#define LOBYTE(word) ((word) & 0xff)
#define HIBYTE(word) (((word) >> 8) & 0xff)
int WSAStartup(WORD requested, WSADATA *data);
int WSACleanup(void);
