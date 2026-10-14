# Mods Agent executor scheduling: Native 2.1.289

This standalone oracle executes the exact Native `ds` streaming tool executor
class and the exact `Tn` completed-result poll extracted from the pinned Claude
Code 2.1.289 binary. It tests queue starts, ready-result selection, discard,
and the provider-event drain boundary without starting the Native CLI or making
a provider request.

Run it with:

```sh
node scripts/tests/mods_agent_executor_schedule_289_oracle.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The probe checks the complete binary SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69` and
checks each source range before evaluation:

| Native source | Byte range | SHA-256 | Use in the oracle |
|---|---:|---|---|
| `ds` streaming executor class | `[195349196,195357760)` | `6459257ae75dd02d6b6f06df7f14bf7ce0468ebea7d3ce7909e239bdd527cba8` | Executed unchanged in a Node VM; includes `addTool`, `processQueue`, `executeTool`, `discard`, `getCompletedResults`, and `getRemainingResults`. |
| `addTool` | `[195350179,195351516)` | `92b720b6a34784e97521f5d290052a628d34aa365dc8b674d7e6fde78f810b2a` | Separately hash-checked against the method inside `ds`. |
| `getCompletedResults` | `[195355929,195356509)` | `39bd63336ce18174bef5b3867e0e6b721153fb64194a1ce144cd8418f2aa2bed` | Separately hash-checked against the method inside `ds`. |
| `Tn` completed-result poll and adjacent finalizer | `[195439370,195440609)` | `d082ae78e6737bc5c45d5adda2d515c8406231b5f3d8e63d23f0ac0041ce3ce7` | The exact `Tn` generator is extracted from the checked window and executed with row-projection seams. |
| Provider event yield, tool enqueue, then `Tn` poll | `[195466181,195469377)` | `380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c` | Statically checks the caller order `yield Me` → `addTool` → `yield*Tn()`. |
| Post-stream `getRemainingResults` drain | `[195485961,195489200)` | `7fcac4553bbb01791b1e0da0e5005fdc6c061a051391aa00de6bc79d42995a47` | Pins the separate final drain after provider iteration. |

The VM replaces Native imports at the executor boundary, not the scheduler.
`yn` resolves the controlled tool definitions; their schemas and
`isConcurrencySafe` values are fixture inputs. `W1` is a controlled
abort-aware async generator that records entry and waits on explicit gates.
The abort, telemetry, and row-projection helpers are deterministic stubs.
The executor's own queue state transitions and scheduling methods remain the
pinned Native implementation. The `processQueue` observation in one test is a
transparent wrapper around the bound Native method; it counts calls and
delegates each call unchanged.

The nine checks establish these behaviors:

1. `addTool` starts its first tool without a `Tn` or `getCompletedResults`
   poll.
2. For an unsafe A with unsafe B queued, A's `executeTool` promise `finally`
   calls `processQueue` and starts B. The test releases A without a provider
   event or result poll between the two starts.
3. When concurrency-safe B finishes before concurrency-safe A and both are
   complete at drain time, `getCompletedResults` yields A then B: ready entries
   are visited in executor registration order, not completion-arrival order.
4. If safe A is still executing while safe B completes, `getCompletedResults`
   skips A and yields B. It does not wait for the earlier safe entry.
5. Calling `getCompletedResults` with only A executing and B queued yields no
   result and does not start B.
6. An executing unsafe B is a result-drain barrier: a completed A before it is
   yielded, but later queued safe C is not started or returned.
7. `discardAndAbortInFlight` aborts A and prevents queued B from entering
   `W1`; B follows the Native discarded-tool result path.
8. A completed row remains inside the executor until the exact Native `Tn`
   poll is invoked. The test holds a provider-event gate, completes A, confirms
   no row projection occurred, then opens the gate and executes `Tn`. The
   source-pinned caller order confirms Native yields the provider row before
   enqueueing its tools and polling `Tn`.
9. The exact Native `getRemainingResults` method drains a completed row and its
   in-progress-ID removal after the stream path ends.

Result order should not be simplified to either “completion arrival” or
“registration order” without the ready-set condition. In registration order,
safe executing entries do not block polling a later completed safe entry. A
currently executing unsafe entry does stop the scan. The source-driven
`addTool`/`processQueue` scenarios cannot produce an already-completed safe
entry *behind* such an unsafe executing entry: the unsafe call prevents later
safe work from entering `W1`. The oracle therefore tests the reachable barrier
state (later safe work remains queued) and does not manufacture an impossible
completed-behind-barrier state by mutating Native executor entries.

The test driver controls W1 and a provider-event gate; it does not execute the
complete Native query generator. Provider transport, real tool implementations,
permissions, and CLI/session startup are outside this oracle. The checked
Native caller window is used to establish the event/poll ordering; the runtime
test executes the exact `Tn` poll and exact executor methods.
