#!/usr/bin/env python3
"""Exercise libgit2's actual error snapshot functions with allocation failures.

The harness substitutes thread-local storage, the allocator and error-message
buffering.  It extracts git_error_save, git_error_restore, git_error_free and
git__strdup from the product sources.  It does not exercise TLS or networking.
"""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
UTIL = ROOT / "third_party/git2-rs/libgit2-sys/libgit2/src/util"
ERRORS = UTIL / "errors.c"
ALLOC = UTIL / "alloc.c"


def function(text, name):
    match = re.search(r"^(?:int|void|char \*)\s*" + name + r"\(", text, re.M)
    if match is None:
        raise AssertionError(f"Missing product function {name}")
    opening = text.index("{", match.start())
    depth = 0
    for position in range(opening, len(text)):
        if text[position] == "{":
            depth += 1
        elif text[position] == "}":
            depth -= 1
            if depth == 0:
                return text[match.start():position + 1]
    raise AssertionError(f"Unclosed product function {name}")


PREFIX = r'''
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef struct { char *message; int klass; } git_error;
static git_error oom_error = {"Out of memory", 1};
static git_error uninitialized_error = {"not initialized", 2};
static git_error tlsdata_error = {"thread-local failure", 3};
static git_error no_error = {"no error", 0};
#define IS_STATIC_ERROR(e) ((e) == &oom_error || (e) == &uninitialized_error || \
 (e) == &tlsdata_error || (e) == &no_error)
#define GIT_ASSERT_ARG(e) do { if (!(e)) return -1; } while (0)
#define CHECK(e) do { if (!(e)) { \
 fprintf(stderr, "check failed at line %d: %s\n", __LINE__, #e); return 1; \
} } while (0)
struct error_threadstate { git_error *last; git_error error; };
static struct error_threadstate state;
static int unavailable, attempts, fail_at, outstanding;
static char restored_message[128];
static struct error_threadstate *threadstate_get(void) {
 return unavailable ? NULL : &state;
}
/* Like git__malloc, a failed allocation sets the static OOM error. */
static void *git__malloc(size_t size) {
 void *ptr;
 if (++attempts == fail_at) { state.last = &oom_error; return NULL; }
 ptr = malloc(size);
 if (!ptr) { state.last = &oom_error; return NULL; }
 ++outstanding;
 return ptr;
}
static void git__free(void *ptr) {
 if (ptr) { --outstanding; free(ptr); }
}
static void set_error(int klass, char *message) {
 if (!threadstate_get()) return;
 snprintf(restored_message, sizeof(restored_message), "%s", message ? message : "");
 state.error.message = restored_message; state.error.klass = klass; state.last = &state.error;
}
void git_error_free(git_error *error);
'''

CASES = r'''
static void reset(git_error *last, int fail) {
 state.last = last; unavailable = attempts = 0; fail_at = fail;
}
int main(void) {
 char message[] = "original diagnostic";
 git_error original = {message, 17}, changed = {"cleanup diagnostic", 19};
 git_error *saved = NULL;
 git_error *statics[] = {&oom_error, &uninitialized_error, &tlsdata_error};

 reset(&original, 0); unavailable = 1;
 CHECK(git_error_save(&saved) == -1 && saved == &tlsdata_error && attempts == 0);
 unavailable = 0; git_error_free(saved);

 for (int i = 0; i < 2; ++i) {
  reset(i ? &no_error : NULL, 0); saved = NULL;
  CHECK(git_error_save(&saved) == 0 && saved == &no_error && attempts == 0);
  state.last = &changed;
  CHECK(git_error_restore(saved) == 0 && state.last == &no_error);
 }
 for (size_t i = 0; i < sizeof(statics) / sizeof(statics[0]); ++i) {
  reset(statics[i], 0); saved = NULL;
  CHECK(git_error_save(&saved) == 0 && saved == statics[i] && attempts == 0);
  state.last = &changed;
  CHECK(git_error_restore(saved) == 0 && state.last == statics[i]);
 }

 reset(&original, 0); saved = NULL;
 CHECK(git_error_save(&saved) == 0 && saved != &original && outstanding == 2);
 CHECK(saved->klass == 17 && saved->message != original.message);
 CHECK(strcmp(saved->message, "original diagnostic") == 0);
 message[0] = 'X';
 state.last = &changed;
 CHECK(git_error_restore(saved) == 0 && outstanding == 0);
 CHECK(state.last->klass == 17 && strcmp(state.last->message, "original diagnostic") == 0);
 message[0] = 'o';

 for (int failure = 1; failure <= 2; ++failure) {
  reset(&original, failure); saved = NULL;
  CHECK(git_error_save(&saved) == -1 && saved == &oom_error);
  CHECK(attempts == failure && outstanding == 0);
  state.last = &changed;
  CHECK(git_error_restore(saved) == 0 && state.last == &oom_error && outstanding == 0);
 }
 puts("git_error_save: normal, static, absent, thread-local and both allocation failures passed");
 return 0;
}
'''


class GitErrorSave(unittest.TestCase):
    def test_snapshot_allocation_lifetime_and_fallback(self):
        errors, alloc = ERRORS.read_text(), ALLOC.read_text()
        bodies = [function(alloc, "git__strdup")]
        bodies.extend(function(errors, name) for name in
                      ("git_error_save", "git_error_restore", "git_error_free"))
        with tempfile.TemporaryDirectory(prefix="git-error-save-") as directory:
            source, binary = Path(directory) / "error_save.c", Path(directory) / "error_save"
            source.write_text(PREFIX + "\n".join(bodies) + CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("GIT_ERROR_SAVE_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)


if __name__ == "__main__":
    unittest.main()
