# nsld Security Guide

`nsld` is a public-internet edge: it terminates TLS for tenant subdomains
and tunnels traffic to whichever `nsl` client owns each domain. The
tunnel itself is mutually authenticated with HMAC + cert pinning, so
**no untrusted machine can register as a tenant**. But the **public
side** is wide open by default — anything that reaches `https://*.{base_domain}`
gets forwarded to the tenant.

That is intentional for the simplest deploy (a hobby / single-user
setup), but for any environment with real users you **must** put an
authentication layer in front of nsld. There are two supported shapes;
pick exactly one.

---

## TL;DR

| Mode                       | Who runs auth?           | nsld config                    | When to pick                                                                                |
|----------------------------|--------------------------|--------------------------------|---------------------------------------------------------------------------------------------|
| **(A) External reverse proxy** | Traefik / Caddy / nginx | `[forward_auth].enable = false` (default) | You already have a reverse proxy or want maximum flexibility (mTLS, SSO, WAF, rate limit). |
| **(B) Built-in forward_auth** | nsld itself             | `[forward_auth].enable = true` | Single-binary deploy, point at any auth webhook (Authelia, oauth2-proxy, custom).            |
| **None (insecure)**        | nobody                   | both off                       | Local dev only. **Do not use in production.**                                                |

Either A or B closes the gap. Without one, treat every `https://...`
URL nsld serves as world-readable.

---

## Option A — External reverse proxy in front of nsld

Terminate TLS at the proxy, do auth there, forward plain HTTP into nsld.

```
  Internet  ──HTTPS──>  Traefik / Caddy / nginx  ──plain HTTP──>  nsld :80  ──QUIC──>  nsl client
                         │
                         └─ TLS + ACME + auth + WAF + rate-limit (operator's choice)
```

In this mode nsld itself has TLS and ACME **disabled** — the proxy does
both. nsld only needs a plain HTTP listener for routed traffic.

### nsld config

```toml
state_dir = "./data"

[server]
listen = ":443"                  # QUIC tunnel plane, public UDP/443
base_domain = "nsl.example.com"

[public]
http_listen = ":80"              # plain HTTP routed to tenants
https_listen = ""                # leave empty — Traefik/Caddy does TLS

[acme]
enable = false                   # Traefik/Caddy issues certs

[forward_auth]
enable = false                   # the proxy does auth, not nsld
```

### Traefik example (forwardAuth + ACME wildcard via dnsall.com httpreq)

```yaml
# docker-compose.yml
services:
  traefik:
    image: traefik:v3.4
    command:
      - --providers.docker=true
      - --entryPoints.websecure.address=:443
      - --certificatesResolvers.le.acme.email=admin@example.com
      - --certificatesResolvers.le.acme.storage=/letsencrypt/acme.json
      - --certificatesResolvers.le.acme.dnsChallenge.provider=httpreq
      - --certificatesResolvers.le.acme.dnsChallenge.disableChecks=true
    environment:
      LEGO_DISABLE_CNAME_SUPPORT: "true"
      HTTPREQ_ENDPOINT: "https://api.dnsall.com"
      HTTPREQ_USERNAME: "your-dnsall-user"
      HTTPREQ_PASSWORD: "your-dnsall-api-key"
    ports: ["443:443", "80:80"]
    volumes:
      - ./letsencrypt:/letsencrypt
      - /var/run/docker.sock:/var/run/docker.sock
    labels:
      # forwardAuth middleware — every match goes here first
      - traefik.http.middlewares.auth.forwardAuth.address=https://auth.example.com/verify
      - traefik.http.middlewares.auth.forwardAuth.authResponseHeaders=X-Auth-User,X-Auth-Groups
      - traefik.http.middlewares.auth.forwardAuth.trustForwardHeader=true

  nsld:
    image: ghcr.io/your-org/nsld
    network_mode: host             # needs UDP/443 for QUIC
    command: serve --listen :443 --base-domain nsl.example.com
    volumes: [./data:/data]
    labels:
      - traefik.enable=true
      - traefik.http.routers.nsld.rule=HostRegexp(`{tenant:[a-z0-9-]+}.nsl.example.com`) || HostRegexp(`{any:.+}.{tenant:[a-z0-9-]+}.nsl.example.com`)
      - traefik.http.routers.nsld.entryPoints=websecure
      - traefik.http.routers.nsld.tls.certResolver=le
      - traefik.http.routers.nsld.tls.domains[0].main=nsl.example.com
      - traefik.http.routers.nsld.tls.domains[0].sans=*.nsl.example.com
      - traefik.http.routers.nsld.middlewares=auth
      - traefik.http.services.nsld.loadBalancer.server.port=80
```

### Caddy example

```caddyfile
*.nsl.example.com {
    tls {
        dns httpreq https://api.dnsall.com
    }
    forward_auth https://auth.example.com {
        uri /verify
        copy_headers X-Auth-User X-Auth-Groups
    }
    reverse_proxy http://127.0.0.1:80
}
```

### nginx example

```nginx
upstream nsld { server 127.0.0.1:80; }

server {
    listen 443 ssl http2;
    server_name *.nsl.example.com;
    ssl_certificate     /etc/letsencrypt/live/nsl.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/nsl.example.com/privkey.pem;

    location / {
        auth_request /_auth;
        auth_request_set $user $upstream_http_x_auth_user;
        proxy_set_header X-Auth-User $user;
        proxy_pass http://nsld;
    }

    location = /_auth {
        internal;
        proxy_pass https://auth.example.com/verify;
        proxy_pass_request_body off;
        proxy_set_header Content-Length "";
        proxy_set_header X-Forwarded-Method $request_method;
        proxy_set_header X-Forwarded-Proto  $scheme;
        proxy_set_header X-Forwarded-Host   $host;
        proxy_set_header X-Forwarded-Uri    $request_uri;
        proxy_set_header X-Forwarded-For    $remote_addr;
    }
}
```

---

## Option B — Built-in `forward_auth`

If you don't want to run a second piece of software, nsld itself can
gate every request against an external auth webhook before tunneling.
Same protocol shape as Traefik's `forwardAuth` — any service that works
there works here.

```
  Internet  ──HTTPS──>  nsld (TLS + ACME + auth)  ──QUIC──>  nsl client
                          │
                          └─ GET /verify → 2xx allow / 3xx redirect / 4xx-5xx deny
```

### nsld config

```toml
state_dir = "./data"

[server]
listen = ":443"
base_domain = "nsl.example.com"

[public]
https_listen = ":443"
http_listen  = ":80"             # 301 → HTTPS

[acme]
enable = true
contact_email = "admin@example.com"
directory = "https://acme-v02.api.letsencrypt.org/directory"
httpreq_url      = "https://api.dnsall.com"
httpreq_username = "your-dnsall-user"
httpreq_password = "your-dnsall-api-key"
propagation_wait_secs = 5
renewal_threshold_days = 30

# === The auth gate ===
[forward_auth]
enable = true
address = "https://auth.example.com/verify"

# Auth-side response headers to copy onto the upstream request
# (so the tenant's app reads them as identity)
response_headers = ["X-Auth-User", "X-Auth-Groups"]

# Extra client-side request headers to forward to the auth webhook
# (Cookie + Authorization are always forwarded, list anything extra)
request_headers = []

# Paths that bypass the gate. ACME http-01 verification is here too,
# in case you ever flip back to that challenge type.
bypass_prefixes = ["/.well-known/acme-challenge/", "/health"]

timeout_secs = 5
tls_verify = true                # off only for self-signed dev auth
```

### Wire shape (what nsld sends to your auth endpoint)

```
GET /verify HTTP/1.1
Host: auth.example.com
X-Forwarded-Method: GET
X-Forwarded-Proto:  https
X-Forwarded-Host:   myapp.alice.nsl.example.com
X-Forwarded-Uri:    /dashboard?id=42
X-Forwarded-For:    203.0.113.7
Cookie: session=...
Authorization: Bearer ...
(any header you list in `request_headers`)
```

Auth response semantics:

| Status     | Action                                                             |
|------------|--------------------------------------------------------------------|
| **2xx**    | Allow. Named `response_headers` are appended to the upstream request before tunneling, so the tenant's app reads e.g. `X-Auth-User: alice`. |
| **3xx**    | Short-circuit. Response is forwarded verbatim to the client (typical login redirect with `Location:`). |
| **4xx/5xx**| Short-circuit. Status + headers + body are sent as-is to the client (`401` with `WWW-Authenticate`, `403`, etc). |
| network error / timeout | **Fail-closed.** Client sees `502 Bad Gateway`. No request reaches the tenant. |

### Compatible auth services

The protocol is the lego/Traefik standard, so:

- **Authelia** — drop-in. Set `forward_auth.address = "https://authelia.example.com/api/verify?rd=https://auth.example.com"`.
- **oauth2-proxy** — use `--reverse-proxy` mode and point at `/oauth2/auth`.
- **Vouch Proxy** — `address = "https://vouch.example.com/validate"`.
- **Pomerium** — use the forward-auth endpoint.
- **Custom** — anything that returns 2xx/4xx with optional headers.

---

## What's covered, what isn't

✅ Covered by either Option A or B:

- Random internet visitors can't reach a tenant's app without passing auth.
- Auth-asserted identity reaches the tenant app via injected headers.
- DPI sees standard HTTPS to a public hostname (no exotic protocol).

❌ Still **your** responsibility:

- **App-level vulnerabilities** behind nsld — auth is identity, not
  authorization-on-resources. Apps must still check what the
  authenticated user is allowed to do.
- **Tenant secret hygiene** — anyone with a tenant's token (`tokens.toml`
  entry's `key`) can register as that tenant and intercept all its
  traffic. Rotate keys on suspicion.
- **Bypass-prefix scope** — anything you list in `bypass_prefixes` is
  reachable without auth. Keep the list to genuinely public paths
  (health checks, ACME challenge verification).
- **Tunnel auth webhook itself** — host it on a public, TLS-served
  endpoint with its own auth (or behind the same reverse proxy).

---

## Migration: from "no auth" to "with auth"

Already running an unprotected nsld? Pick one of:

**Cheapest (Option B):**
1. Stand up an auth webhook (Authelia is ~50 lines of YAML to start).
2. `[forward_auth].enable = true` + `address = "https://auth..."` in nsld config.
3. Reload nsld. Existing tunnels keep working; the next public request goes through auth.

**Most flexible (Option A):**
1. Stand up Traefik/Caddy on the same host.
2. Switch nsld to `[public].https_listen = ""`, `[acme].enable = false`,
   `[forward_auth].enable = false`. nsld now serves plain HTTP on `:80`
   to the proxy.
3. Repoint your DNS / firewall so external `:443` lands on the proxy,
   not nsld.

In both cases the change is invisible to the `nsl` client side — no
client config needs to change.

---

## Distribution

Each tagged release ships two ways:

### Binary tarballs (GitHub Releases)

```
nsld-linux-x64.tar.gz       nsld-linux-arm64.tar.gz
nsld-darwin-x64.tar.gz      nsld-darwin-arm64.tar.gz
nsld-win32-x64.zip
```

Drop `nsld` next to a `data/` directory containing your `config.toml`
and `tokens.toml`, then `./nsld serve`. State (identity, tokens,
ACME / default-CA) lives under `./data` (override with `--state-dir`).

### Docker image (Docker Hub)

Multi-arch (linux/amd64 + linux/arm64) image, built on Alpine 3.22
with ca-certificates + tini as PID-1. Published by `.github/workflows/docker.yml`:

```
docker.io/nsiod/nsld:<version>      # e.g. nsiod/nsld:v0.1.5
docker.io/nsiod/nsld:latest
```

```bash
mkdir -p data
# Drop a config.toml + tokens.toml into ./data first.
docker run -d --name nsld \
  -p 80:80 -p 443:443 -p 443:443/udp \
  -v "$PWD/data:/data" \
  nsiod/nsld:latest serve
```

Notes:

- The container runs as **root** by default so it can bind 80/443.
  Override with `--user 65534:65534` (Alpine's `nobody` UID) if you
  don't need privileged ports.
- `:443/udp` is essential — that's the QUIC tunnel plane.
- `state_dir` defaults to `./data` inside the container; the volume
  mount keeps `identity.pem`, `tokens.toml`, `acme/`, and
  `default-certs/` on the host across restarts.
- The image ships `ca-certificates` so ACME / forward-auth / DNS-01
  webhook calls to public HTTPS endpoints work out of the box.
- `docker exec -it nsld sh` works for debugging — Alpine has a
  full BusyBox shell inside.
- If you put Traefik / Caddy in front (Option A above), don't publish
  `:443/tcp` from this container — let the reverse proxy own that
  port and forward to nsld's `:80`.

To publish your own image in a fork, set two repo secrets and update
`IMAGE_NAME` in `.github/workflows/docker.yml`:

| Secret | Value |
|--------|-------|
| `DOCKER_USER` | Docker Hub account name |
| `DOCKER_PWD`  | Docker Hub access token (Account Settings → Security → New Access Token) |
