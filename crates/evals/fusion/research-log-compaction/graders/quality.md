---
type: llm
name: quality
weight: 2.0
---
The load-bearing question is what happens to a consumer holding an offset below the
reclaimed region. A strong answer distinguishes designs that keep offsets valid from
those that invalidate them, and says how a consumer discovers that its pointer is gone.
Look for: segment-level deletion versus in-place rewriting; whether a reader can tell
"my offset was compacted" apart from "my offset is corrupt"; indirection through a
logical sequence number; and the cost of the index a logical scheme needs. An answer
that only says "delete old segments" without addressing the stranded consumer is weak.
