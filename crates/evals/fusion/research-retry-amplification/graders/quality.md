---
type: llm
name: quality
weight: 2.0
---
The core is that per-layer retry counts multiply rather than add. A strong answer computes
or at least states that product, and then covers the mechanisms that actually bound it:
retry budgets expressed as a fraction of traffic, deadline propagation so an inner retry
cannot outlive the caller's patience, circuit breaking, and retrying only at one layer by
policy. Look for the observation that jitter alone changes timing but not total volume.
An answer that recommends exponential backoff as the fix without noting it does not bound
the total should score low.
