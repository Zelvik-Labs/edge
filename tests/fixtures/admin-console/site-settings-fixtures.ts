// Captured contract from the admin console's site settings module.
//
// console-repository: Zelvik-Labs/admin-console
// console-revision: 8652b830f83dc7ffd1f77bea5d017dcb77f972ad
// console-path: src/features/sites/site-settings-fixtures.ts
//
// The edge asserts against the two declarations it depends on. Re-capture and
// bump the revision line whenever the console changes them; do not edit the
// declarations to match the edge.

export type HttpRedirectStatus = 301 | 302 | 303 | 307 | 308;

export const HTTP_REDIRECT_STATUSES: readonly HttpRedirectStatus[] = [301, 308, 302, 303, 307];
