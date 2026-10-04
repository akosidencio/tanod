# A real Next.js build, trimmed to its manifests

Verbatim output of `next build` on the storefront in [`fixtures/next-storefront`](../../../../../fixtures/next-storefront)
— Next 16.3.3, App Router plus Pages Router — reduced to the four files
`@tanod/next` actually reads. About 6 KB in total.

**Committed on purpose.** The real build output is `.gitignore`d, so tests that
read it pass on a machine that has run `next build` and fail everywhere else.
That is exactly what happened: the package's first CI run failed on
`could not read .../.next/BUILD_ID` while every local run was green.

Copied rather than hand-written, so the compatibility matrix in
[`src/compat.js`](../../../src/compat.js) is asserted against a format Next
really emitted rather than one somebody transcribed. Regenerate by running
`next build` in the storefront and copying these four files across; the
manifest versions recorded in `compat.js` must be updated to match.
