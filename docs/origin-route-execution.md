# Per-deployment origin routing

The management plane may supply `reverseProxy.originRoute` in the existing server configuration JSON. It is an execution plan for the server's existing origin and cache runtime. It does not add a second public site/GTM name or a second agent channel.

The edge validates every plan before publishing any part of a server snapshot. Invalid identities, expired leases, public-hostname loops, disabled TLS verification, unauthorized direct fallback, and shield-to-shield paths reject the snapshot and leave the installed generation intact. Runtime expiry stops selecting that plan's candidates.

## Execution fields

```json
{
  "siteId": "site-a",
  "deploymentId": "deployment-a",
  "revision": 1,
  "validUntil": 1900000000,
  "mode": "smart_shield",
  "role": "ingress",
  "cacheNamespace": "content-version-a",
  "allowDirectFallback": false,
  "selectedPathId": "hong-kong",
  "holdUntil": 0,
  "measurementMaxAgeSeconds": 90,
  "minImprovementPercent": 10,
  "maxAttempts": 2,
  "paths": [{
    "id": "hong-kong",
    "kind": "shield",
    "priority": 1,
    "ready": true,
    "validUntil": 1900000000,
    "observedAt": 1899999950,
    "ttfbMicros": 30000,
    "origins": [{
      "id": 1001,
      "isOn": true,
      "weight": 1,
      "addr": "https://192.0.2.10:443",
      "requestHost": "content.example",
      "tlsServerName": "shield.example",
      "originAuthentication": {
        "deploymentId": "deployment-a",
        "token": "resolved-secret-value-at-least-32-visible-ascii-bytes"
      }
    }]
  }]
}
```

IDs, readiness, measurements, leases and namespaces are compiled by the controller from authorized deployments, configuration application and observations. They are not accepted from visitor headers. `OriginConfig.id` must be stable and unique to the service/deployment endpoint; sharing an IP does not justify sharing a health identity across sites.

`direct` uses customer-origin paths. `fixed_shield` uses explicit shield priorities. `smart_shield` compares fresh, comparable TTFB observations, observes the configured hold interval and minimum improvement, and otherwise uses the static priority path. These are observations of the declared path direction, not a claim to know every external CDN PoP's network performance. Unavailable paths select the next authorized healthy priority; all failed paths return no backend. Managed routes do not revive down origins or use the older node-global L1/L2 fallback.

The existing consistent-hash origin selector receives the complete cache object key. Non-cacheable requests use a stable server-and-URI key. Each configured path has its own origin pool, allowing the request-time selector to react to health and measurement/lease expiry without building a new pool on the request path.

## Protected shield ingress

A shield server also supplies `originIngress`:

```json
{
  "siteId": "site-a",
  "deploymentId": "deployment-a",
  "hostname": "content.example",
  "tokenSha256": "64-hex-digit-sha256-of-the-resolved-secret",
  "allowedCidrs": ["192.0.2.0/24"],
  "validUntil": 1900000000
}
```

Its origin route must have matching site/deployment identity, `role: shield`, `mode: direct`, and direct customer-origin paths. Protected ingress cannot enable listener PROXY-protocol rewriting. Location-specific origin plans must keep the same protected identity and shield role.

Requests must match the configured hostname, deployment header, source network and token hash and arrive over a real TLS transport. Visitor URI schemes and forwarded-protocol headers cannot satisfy TLS authorization. The source is the transport peer exposed by the session, before X-Forwarded-For or client-IP restoration. The controller must configure the cloud/custom-origin path to send `X-Zelvik-Origin-Token` and `X-Zelvik-Origin-Deployment`; direct edge-to-shield requests add `X-Zelvik-Origin-Hop: 1` as well. A hop value other than 1 is rejected. Values are stripped before request logging or customer-origin forwarding, including rejected requests. Outgoing shield authentication is rebuilt exclusively from the selected origin's configured authentication. Secret debug representations are redacted.

TLS certificates are checked independently from HTTP Host. `tlsServerName` controls SNI, while `requestHost` controls the upstream Host header. Shield connections require HTTPS, authentication, and certificate validation.

## Cache and retry behavior

Managed cache keys are scoped by site identity and a length-prefixed content-equivalence namespace, including purge keys. The same site's cloud deployments can share content only when the controller explicitly assigns the same namespace and equivalent key/response semantics. A different site or content namespace stays isolated. Cache eligibility, response privacy, Age/Date freshness, request collapse and purge barriers continue through the existing cache path.

Connection-failure retries are allowed only for GET/HEAD and the configured attempt budget (default two, maximum three). POST, PUT, uploads and other potentially side-effecting requests are not automatically replayed. This does not migrate existing connections or provide a cross-provider atomic cache purge; the controller orchestrates those operations and reports their actual results.

Verification resides in `origin_route::tests`, `origin_route::runtime_tests`, the protected origin proxy callback test, and the expired-plan snapshot test. Cloud account and end-to-end multi-node verification must additionally exercise the control-plane-generated plan; local unit/runtime tests do not prove cloud-side deployment readiness.

## Cloud configuration confirmation

The cloud adapters expose `ConfigureOrigin(ctx, spec)` and the read-only `VerifyOrigin(ctx, spec)`. An accepted mutation remains pending. The latter compares the actual endpoint, protocol, requested Host/SNI and all three owned authentication headers; old values, absent rules, disabled rules and ambiguous ownership cannot confirm application. Runtime `originRoutes` evidence is generated from the edge's published compiled pools and includes the server, site, deployment, revision, role and lease. Desired configuration is not execution evidence.

| Product | Managed update and readback |
| --- | --- |
| Tencent CDN | DescribeDomainsConfig / UpdateDomainConfig, preserving unrelated Origin and RequestHeader properties |
| Alibaba CDN | DescribeCdnDomainDetail + DescribeCdnDomainConfigs, ModifyCdnDomain + BatchSetCdnDomainConfig, reusing ConfigId |
| Huawei CDN | Domain full-config GET/PUT, typed sources.host_name, SNI and merged origin request headers |
| Cloudflare for SaaS | Per-custom-hostname origin/SNI plus individually owned hostname-scoped Transform/Origin rules; verified HTTPS-only origin pull required |
| Alibaba ESA | Individually owned hostname-scoped Origin and Request Header Modification rules, with exact rule identity |
| Tencent EdgeOne | Acceleration domain origin plus one owned L7 rule; SNI must follow the configured origin Host |

Cloudflare uses a hostname-scoped Configuration Rule with `ssl: origin_pull` when the zone does not already enforce SSL-only origin pull. Full (strict) follows the visitor scheme and is insufficient by itself; the HTTPS-only setting must be read back before installing authentication headers. Cloudflare updates never replace the SaaS zone fallback or a complete shared ruleset. Required rule phases and product permissions must exist; provider errors remain visible. Wildcard rules need an explicitly scoped matching plan and are rejected where only exact-host rules have been implemented. Unsupported SNI combinations, conditional/shared origin rules and incomplete inventories are explicit blockers.

Switching an authenticated cloud path to a direct origin must remove its old credentials before changing the endpoint. Tencent/Huawei remove only the owned headers while preserving other settings. Alibaba CDN, Cloudflare, ESA and EdgeOne remove only exactly owned function/rule IDs and use a pure read to confirm absence before changing the endpoint. An asynchronous deletion keeps the operation pending and retryable. Shared or conditionally scoped operator changes block deletion; a token is never knowingly forwarded to a customer origin during the transition.

Provider contract tests use signed HTTP transport with simulated official API responses, including delayed configuration readback. They do not require cloud credentials and do not perform live cloud writes. Public TLS/probe validation and edge application evidence are additional gates after configuration readback.

## Actual path observations

`originPathObservations` reports observations of managed upstream GET/HEAD attempts. Timing starts after selecting the actual origin and ends at upstream response headers, so it excludes visitor queueing and cache hits. Connection/proxy failures and upstream 5xx responses count as errors; no error is recorded as zero latency. Each record identifies the active site, deployment, server, revision, role and actual path, including a backup chosen after a failure.

Smart routes also sample every compiled candidate from the executing Edge node every five seconds, including candidates that receive no visitor traffic. These authenticated HEAD requests connect directly to the compiled endpoint with the same Host, SNI, strict TLS verification and deployment authentication; they do not traverse visitor routing, public GTM or the object cache. The configured origin health URL contributes only its path and query. A fixed transport and concrete Host are required; unsupported client-identity or visitor-dependent transport produces no fabricated quality result. Global probe concurrency is bounded, each path has at most one queued or running sample, and queued work retains weak runtime ownership and rechecks the published generation and lease before network I/O.

Active samples use `sourceKind: active_origin` in a separate rolling window. Their TTFB ends when response headers arrive. A HEAD rejection such as 401 or 405 is insufficient quality evidence and does not change business health, LB availability or the configured GTM health policy. Without comparable fresh successful samples the static priority remains in effect. Passive GET/HEAD samples continue to use `passive_origin`; the two kinds are never combined into one window.

The statistics use a rolling 90-second window with per-second buckets. `ttfbMicros` is the arithmetic mean of successful attempts, `observedAt` is the most recent successful sample's Unix second, and `lastAttemptAt` includes failures. Counts distinguish success and error samples. Empty windows produce no observation. Four local shards record samples without request-path allocation or a global reporting lock. Weak observer ownership retains samples across identical execution plans and lease renewals; a different revision or endpoint starts a separate window.

The authenticated node report supplies the executor identity. Direction is `ingress_to_shield`, `shield_to_origin` or `ingress_to_origin`; there is no inferred cloud-PoP-to-shield measurement. Loaded route evidence also reports the plan's `selectedPathId` and `holdUntil`. The controller requires sufficient comparable coverage before sending dynamic path costs. Paths without traffic remain unmeasured and use static priorities; this mechanism does not send exploratory visitor requests or claim an optimal path from missing data.

Protected local preheat uses an executor-minted, one-shot capability bound to the server, HTTP Host and request target. Only an actual loopback peer may consume it. HTTPS preheat verifies the server's loaded certificate, preserves the ingress HTTP Host while selecting the corresponding TLS name, disables environment proxying and refuses redirects. URL/directory purge tasks use the same origin namespace and generation barriers as normal requests. A failed completion RPC remains unacknowledged and requires retry.
