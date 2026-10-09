# Native 2.1.295 tool_result `is_error` presence fixtures

This fixture tree contains two bounded isolated public CLI runs against the pinned Native 2.1.295 executable. The MCP run calls three local tools whose JSON-RPC result differs only in `isError` omitted/false/true; the Bash control runs a harmless fixed `printf` command. Both use loopback-only Anthropic Messages stubs, dummy API keys, fresh HOME/config directories, and no upstream requests. Raw baselines and SHA-256 manifests are under `native-baseline/2.1.295/`.

Observed model-facing behavior: MCP omitted and MCP `isError:false` both omit `is_error`; MCP `isError:true` emits `is_error:true`. The independent successful Bash result emits explicit `is_error:false`. This is evidence for distinct MCP and Bash mapping behavior; it does not support globally stripping explicit false. See each baseline receipt for exact raw byte paths, field order, run exit/result, and scope.
