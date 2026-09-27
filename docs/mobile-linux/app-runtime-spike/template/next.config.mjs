// Minimal config for the local-apps phase-0 app-runtime spike.
//
// `next dev` runs with the config as-is. The export-build measurement runs
// `NEXT_OUTPUT=export next build`, which switches on `output: 'export'` and
// writes the static site to out/. Keeping the toggle in the environment lets
// one template serve both measurements without editing files mid-run.
const nextConfig = {
  ...(process.env.NEXT_OUTPUT === 'export' ? { output: 'export' } : {}),
};

export default nextConfig;
