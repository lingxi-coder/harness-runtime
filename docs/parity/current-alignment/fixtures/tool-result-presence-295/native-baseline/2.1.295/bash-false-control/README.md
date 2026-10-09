# Native 2.1.295 Bash success `is_error:false` control

The public CLI used its built-in Bash tool to run only `printf NATIVE_BASH_TOOL_RESULT_FALSE_CONTROL` in a fresh temporary working directory. The API server was a loopback stub with a dummy key and returned no upstream requests. The raw model-facing Bash `tool_result` includes explicit `is_error:false`, with key order `tool_use_id,type,content,is_error,cache_control`. All bytes and the original capture receipt are hashed in `manifest.json`; isolated config is omitted.
