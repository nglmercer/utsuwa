# Deploying the web build

The web build is a single-user app with **no login**. On `localhost` that is
fine. Anything else needs the measures below.

## Never expose the app bare

Every `/api/*` route (chat, provider keys, MCP proxy, media) trusts the
network. Before listening on a non-loopback address, put authenticated
ingress in front: Tailscale/WireGuard, a VPN, or a reverse proxy with auth
(basic auth at minimum, OAuth/OIDC better). There is no in-app session to
fall back on.

## MCP proxy (`MCP_ENABLED=server`)

Enabling the proxy turns the server into an MCP client that dials whatever
URLs the browser sends (DNS-rebinding guarded, but otherwise open):

- Set `MCP_HTTP_ALLOWED_HOSTS` to the exact hosts you use. Unset means any
  host — acceptable on localhost, wrong on a shared network.
- Pin every stdio server in `MCP_STDIO_SERVERS` plus
  `MCP_STDIO_ALLOWED_COMMANDS`. Unset refuses all stdio servers (fail-closed).
- Cross-site browser calls are rejected by an `Origin` check in
  `src/hooks.server.ts`, but that only stops browsers — it is not access
  control. Authenticated ingress (above) is what keeps strangers out.

## Rate limiting behind a proxy

`src/hooks.server.ts` rate-limits API mutations per client IP, and the IP
comes from the adapter's `getClientAddress()`:

- Behind a reverse proxy with default settings that address is the **proxy's**
  socket address, so all clients share one bucket: one abuser throttles
  everyone. Configure your adapter to take the client IP from the header
  your proxy sets (on adapter-node: the `ADDRESS_HEADER` / `XFF_DEPTH` env
  vars), and have the edge strip or overwrite that header so clients cannot
  spoof it.
- Buckets are in-memory and per-process. A multi-instance deployment needs a
  shared limiter (or the authenticated ingress above) instead of relying on
  these counters.

## Native desktop build

The Rust host (`UTSUWA_NATIVE=1`) serves no network ports: the UI runs in a
local webview over `companion://app` and MCP/child-process execution stays
behind the OS-level approval dialog. None of the ingress guidance above
applies to it.
