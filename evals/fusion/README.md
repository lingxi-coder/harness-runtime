# Fusion evaluation corpus

Twenty-four tasks, six each in research, planning, code review, and patch
proposal. Every task is asked twice: once of a single model, once through
`/fusion` with three models from different providers. Everything else is
shared, so the pair is comparable and the difference between the two runs is
attributable to the arm rather than to the setup.

Run it:

```
lingxi plugin eval --eval-dir evals/fusion --ablation none --max-cost-usd 20 .
```

`--ablation none` matters: the arms below replace the with-plugin/without-plugin
pair, and asking for both would double the runs without adding a comparison.

## What the numbers mean

The `single` arm is the baseline. `delta` is the fusion arm's score minus it,
and `perArm` carries each arm's own score. A case scores through two graders:
`quality` rates the answer against the task's own rubric, and `vs-baseline`
compares the two arms directly. The second one is why the arms have to be
paired: it reads the baseline arm's output.

## Writing a case

Nothing in a prompt may hint at what the rubric rewards. The rubrics name the
specific things a good answer contains, and they are deliberately not derivable
from the task text: an answer that scores well has to have found them. Rubrics
also name what should count against an answer, because a longer answer is not a
better one and a confidently stated non-defect is worse than silence.

The arms differ in exactly two ways: the prompt, and the settings. Graders see
the shared case prompt, never an arm's override, so both arms are judged
against the same task.

## Cost

Twenty-four cases, two arms, two runs each is ninety-six agent runs plus two
graders per run. The judge draws on the same `--max-cost-usd` pool as the runs
it grades, so a ceiling that is too low starves the graders rather than the
runs. Twenty dollars is sized for one full pass; `runs: 2` in each case keeps
it there.

## Known gap

There is no record and replay, so every pass costs real provider calls and no
two passes are identical. Comparing across passes means comparing means, not
individual scores.
