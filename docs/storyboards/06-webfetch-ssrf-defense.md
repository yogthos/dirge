# Storyboard 06 — Webfetch SSRF defense (three layers)

## Scenario

The agent is reasoning about a bug and decides to consult a doc page.
It tries `webfetch https://example.com/docs/some-page`. Fine,
allowed.

A turn later the LLM has been prompt-injected through some external
content (or the model just hallucinated a bad URL). It tries:

1. `webfetch http://169.254.169.254/latest/meta-data/iam/security-credentials/`
   (raw AWS metadata IP)
2. `webfetch http://2852039166/` (decimal-encoded 169.254.169.254)
3. `webfetch http://attacker-controlled.com/innocent` where
   `attacker-controlled.com` DNS-resolves to `169.254.169.254`

All three are blocked. The first by literal-IP check, the second by
the alternate-IPv4 parser, the third by the DNS-pre-resolution
check AND (as a backstop) the custom `dns_resolver`.

## What the user sees

For each blocked request:

```
[@@] running: webfetch
  > {"urls": ["http://169.254.169.254/latest/meta-data/iam/security-credentials/"]}

  ✗ tool error:
    webfetch refused "http://169.254.169.254/latest/meta-data/iam/security-credentials/":
    host 169.254.169.254 resolves to a private/loopback/link-local address.
    Set DIRGE_WEBFETCH_ALLOW_PRIVATE=1 to allow this.
```

The error surfaces to the LLM via the tool-result message, so the
model knows the request was refused.

## Code trace

### Step 1 — `WebFetchTool::call` invoked

- `src/agent/tools/webfetch.rs:505` (`impl Tool for WebFetchTool`)
  receives the JSON args, asserts non-empty URLs, then builds a
  `reqwest::Client` with the SSRF-defending configuration (custom
  `dns_resolver` at line 562, redirect policy at 563-576) before
  iterating the URLs.

### Step 2 — Layer 1: literal-host check

- `fetch_url` → `validate_url_host_safety(url)`
  (`src/agent/tools/webfetch.rs:80-145`):
  - Strips the scheme (case-insensitive `https://` / `http://`)
  - Extracts the host (bracket-aware for IPv6)
  - Hostname blocklist: `localhost`, `ip6-localhost`,
    `ip6-loopback`
  - `host.parse::<IpAddr>()` → if success, `is_private_or_loopback`
  - Fallback: `parse_alt_ipv4` for decimal/octal/hex IPv4
    (`http://2852039166/`, `http://0x7f.0.0.1/`)
- `169.254.169.254`: parses as `Ipv4Addr` → `is_link_local()` →
  blocked. ✓
- `2852039166`: dotted parse fails → `parse_alt_ipv4` decodes the
  decimal form to `[169, 254, 169, 254]` → blocked. ✓
- `http://attacker-controlled.com/…`: literal host is a domain
  name; parse fails; alt_ipv4 fails (not all-digits). Passes
  layer 1.

### Step 3 — Layer 2: pre-request DNS resolution

- `resolve_and_validate_host(url)` (`src/agent/tools/webfetch.rs`,
  TOOL-1 fix):
  - Honors `DIRGE_WEBFETCH_ALLOW_PRIVATE=1` opt-out.
  - Skips IP literals (already covered by layer 1).
  - `tokio::net::lookup_host("attacker-controlled.com:443")`.
  - Filters resolved `SocketAddr`s through `is_private_or_loopback`.
  - If ANY resolved address is blocked, returns
    `Err("...resolved to private/loopback address ...")`.
- This catches the DNS rebinding case: the attacker's domain
  resolves to a private IP at lookup time. ✓

### Step 4 — Layer 3: custom resolver pins the validation

- The reqwest `Client::builder` is configured with:
  ```rust
  .dns_resolver(Arc::new(ValidatingResolver))
  ```
- `ValidatingResolver` (`src/agent/tools/webfetch.rs`) implements
  `reqwest::dns::Resolve`. Every TCP connect — initial AND
  redirects — goes through it.
- Even if the layer-2 pre-resolution succeeded against a cached
  benign IP and the TTL expired mid-fetch (DNS rebinding past
  the cache), the connect-time resolution is re-checked.
- Returns `PermissionDenied` with a clear "blocked by SSRF guard"
  message when all resolved addrs are private.

### Step 5 — Redirect policy as a fourth backstop

- `reqwest::redirect::Policy::custom` re-runs
  `validate_url_host_safety` on every hop's URL.
- Max 10 hops.
- An attacker who controls a public page that 302s to
  `http://169.254.169.254/...` is stopped here even if the public
  page passed layers 1-3.

## Why all four layers?

| Attack | Layer that blocks |
|---|---|
| Direct IP literal (`http://127.0.0.1/`) | 1 |
| Alt-form IPv4 (`http://2852039166/`) | 1 |
| IPv4-mapped IPv6 (`http://[::ffff:127.0.0.1]/`) | 1 |
| Hostname → private IP at request time | 2 |
| Hostname → benign IP at validation, attacker swaps DNS before connect | 3 |
| Public page 302 → private IP | 4 |

Defense in depth: layers 1 and 2 are fast and produce clear error
messages. Layer 3 is the security backstop that catches anything
the prior layers miss. Layer 4 is the redirect guard.

## Coverage

- `validate_url_host_safety_blocks_ssrf_targets` in `src/agent/tools/webfetch.rs`
  exercises layer 1 against a dozen IPv4/IPv6 forms.
- `validate_url_host_safety_handles_malformed_hosts` confirms
  malformed input fails closed (refused, not silently allowed).
- `resolve_and_validate_host` + `ValidatingResolver` are exercised
  by the live webfetch path; the unit tests cover the helper
  predicates (`is_private_or_loopback`, `is_ipv4_mapped_private`).

## Edge cases verified

- **`DIRGE_WEBFETCH_ALLOW_PRIVATE=1`**: the user's opt-out is
  honored at EVERY layer (literal check, pre-resolve, custom
  resolver). Necessary for legitimate workflows like fetching
  from `http://localhost:8080/` during local development.
- **Unresolvable hostname**: `resolve_and_validate_host` returns
  `Ok(())` — let reqwest surface the canonical network error
  rather than masking it.
- **Mixed resolved addrs (one public, one private)**: layer 2
  rejects on ANY private address. Strict — won't trust the
  "public one" because at connect time reqwest could pick either.
- **Cloudflare-fronted public hostname**: resolves to a CF IP
  (public range) → passes layers 1-3 → connects normally. If
  the page 302s to a private IP → layer 4 catches it.
