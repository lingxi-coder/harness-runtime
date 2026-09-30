---
type: baseline
name: vs-baseline
weight: 1.0
---
Score the candidate higher only when it finds something real the baseline missed, or when
it corrects a claim the baseline made that is wrong. A finding that is not actually a
defect counts against the candidate, however confidently it is stated. Penalise
restatement of the code, style commentary that changes no behaviour, and any suggestion
that would introduce a new defect. Equal quality scores 0.5.
