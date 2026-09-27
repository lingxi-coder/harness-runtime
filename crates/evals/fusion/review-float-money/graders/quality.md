---
type: llm
name: quality
weight: 2.0
---
The defect that matters is that the shares need not sum to the total: rounding each share
independently loses or invents cents. A strong review shows a case where it happens and
proposes computing in integer cents and distributing the remainder explicitly. Another
genuine point: binary floating point cannot represent most decimal amounts exactly, so
`total_dollars` was already approximate before the division. Penalise a review that
proposes only `Decimal` without addressing the sum-preservation problem, since rounding
each share independently still loses cents in decimal arithmetic.
