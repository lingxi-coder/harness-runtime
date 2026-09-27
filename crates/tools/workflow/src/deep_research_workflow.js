export const meta = {
  name: 'deep-research',
  description: 'Research a question across independent sources, verify each claim by vote, and synthesize a cited answer.',
  phases: [
    { title: 'Scope' },
    { title: 'Search' },
    { title: 'Fetch' },
    { title: 'Verify' },
    { title: 'Synthesize' },
  ],
};

const VOTES_PER_CLAIM = 3;
const MAX_FETCH = 15;
const question = typeof args === 'string' ? args : JSON.stringify(args ?? {});

phase('Scope');
const scope = await agent(
  `Turn this research request into a precise scope, subquestions, and source-quality criteria:\n\n${question}`,
  { label: 'scope', phase: 'Scope' },
);

phase('Search');
const searches = await parallel([
  () => agent(`Find primary and authoritative sources for this question. Return URLs and the claims each source can support.\n\nQuestion: ${question}\nScope: ${scope}`, { label: 'primary-sources', phase: 'Search' }),
  () => agent(`Find independent sources that challenge likely answers to this question. Return URLs and the disputed claims.\n\nQuestion: ${question}\nScope: ${scope}`, { label: 'counter-sources', phase: 'Search' }),
  () => agent(`Find recent sources and identify which facts are time-sensitive. Return URLs and publication dates.\n\nQuestion: ${question}\nScope: ${scope}`, { label: 'recent-sources', phase: 'Search' }),
]);

phase('Fetch');
const fetchPlan = await agent(
  `Deduplicate these search results and choose at most ${MAX_FETCH} sources. Return a numbered fetch plan with URL, source type, and claims to inspect.\n\n${JSON.stringify(searches)}`,
  { label: 'fetch-plan', phase: 'Fetch' },
);
const fetched = await agent(
  `Fetch and extract the evidence from this plan. Never inspect more than ${MAX_FETCH} sources. Preserve URL, title, date, and a concise paraphrase of the evidence for every source.\n\n${fetchPlan}`,
  { label: 'fetch-evidence', phase: 'Fetch' },
);

phase('Verify');
const claims = await agent(
  `Extract the material factual claims needed to answer the question. For each claim, include the supporting source URLs and any conflicting evidence.\n\nQuestion: ${question}\nEvidence: ${fetched}`,
  { label: 'claims', phase: 'Verify' },
);
const votes = await parallel(
  Array.from({ length: VOTES_PER_CLAIM }, (_, index) => () =>
    agent(
      `Independently verify every proposed claim. Vote supported, disputed, or insufficient; explain source quality and citation fit. You are verifier ${index + 1} of ${VOTES_PER_CLAIM}.\n\n${claims}`,
      { label: `verify-${index + 1}`, phase: 'Verify' },
    ),
  ),
);

phase('Synthesize');
return await agent(
  `Answer the user's question from the verified evidence. Include direct source URLs next to supported claims, surface disagreements, and omit claims without enough votes. Treat a claim as verified only when a majority of ${VOTES_PER_CLAIM} independent verifier votes support it.\n\nQuestion: ${question}\nClaims: ${claims}\nVotes: ${JSON.stringify(votes)}`,
  { label: 'synthesis', phase: 'Synthesize' },
);
