// The two marker strings below are load-bearing for the measurements:
//
//   SPIKE_PAGE_OK      — scripts/guest/measure-spike.sh polls the served HTML
//                        for this string to detect "dev server ready".
//   SPIKE_HMR_TOKEN_*  — measure-spike.sh rewrites the token with sed and
//                        polls until the new token is served, timing one HMR
//                        edit round-trip. Do not rename either marker without
//                        updating measure-spike.sh.
export default function Home() {
  return (
    <main>
      <h1>hello-next-spike</h1>
      <p data-spike="ready">SPIKE_PAGE_OK</p>
      <p data-spike="hmr">SPIKE_HMR_TOKEN_0</p>
    </main>
  );
}
