# Pino as a Docker Sandboxes kit (`pino-cache`)

Runs `pino-proxy` as an **in-container sidecar** in front of Claude Code inside a
Docker Sandbox. Claude talks to pino on loopback; pino injects prompt-cache
breakpoints + forces a 1h TTL (the ~90% savings) and forwards to
`api.anthropic.com` **through the sandbox proxy**, which still injects the real
Anthropic credential.

```
claude ──http──▶ 127.0.0.1:8898 (pino, in-container)         ← loopback, in NO_PROXY,
                   │  pino rewrites /v1/messages (AUTO_CACHE)    bypasses sandbox proxy
                   ▼
                 api.anthropic.com ──via HTTPS_PROXY──▶ sandbox MITM
                                      swaps x-api-key: proxy-managed → REAL cred ──▶ Anthropic
```

Auth is untouched: the request still terminates at `api.anthropic.com` from the
sandbox proxy's view, so credential injection works exactly as it does today.
**No `ANTHROPIC_BASE_URL` pointing at the host, no service-mapping, no key in the
container.**

## One-time: build & push the pino image

The kit pulls the `pino-proxy` binary out of an OCI image. Build and push it
multi-arch (so it works on both Apple-silicon and amd64 sandboxes) with the
repo Makefile:

```bash
cd ~/workspace/pino
make login    # docker login docker.io  (first time only)
make push     # buildx build linux/amd64,linux/arm64 → docker.io/longnguyen58445/pino:sbx
```

`make push` builds the repo `Dockerfile` (final image has the binary at
`/usr/local/bin/pino-proxy`) and pushes both arches under one tag. The image
must be **public**, or configured with registry creds the sandbox can use.
Override the target with `make push IMAGE=... TAG=...` (keep it in sync with
`PINO_IMAGE` in `sbx-kit/spec.yaml`).

> The build includes a small change to `src/server.rs`: the outbound HTTPS client
> now honors `SSL_CERT_FILE`, so pino trusts the sandbox's MITM proxy CA. Without
> it, pino's call to `api.anthropic.com` fails the TLS handshake inside the sandbox.
> `docker build` compiles this, so a successful `make push` is also the compile check.

## Use it

```bash
sbx create claude --kit ~/workspace/pino/sbx-kit <workspace>
```

(`--kit` is experimental; takes a directory path, OCI ref, or zip. Repeatable.)

Verify inside the sandbox:

```bash
# pino is listening and Claude points at it
cat /proc/$(pgrep -x claude)/environ | tr '\0' '\n' | grep ANTHROPIC_BASE_URL  # http://127.0.0.1:8898
curl -s http://127.0.0.1:8898/__pino/ | head            # savings dashboard
cat /var/log/sbx-kit-startup.log                          # sidecar startup logs
```

## Requirements / caveats

- **The sandbox must have an Anthropic credential** (OAuth login or API key) so the
  sandbox proxy has something to inject — i.e. `SBX_CRED_ANTHROPIC_MODE` is `oauth`
  or `apikey`, **not `none`**. Pino is transparent; it does not supply auth.
- Cache-only by default (`AUTO_CACHE` + `TAIL_TTL=1h`). The riskier body transforms
  (`TRANSFORM=1`: drop-tools / restructure) are **off** — enable in `spec.yaml` only
  after validating against your traffic.
- The sidecar is launched detached at boot and is **not auto-restarted** if it
  crashes. Check `/var/log/sbx-kit-startup.log`.
- Dashboard writes SQLite to `/home/agent/.pino`. Drop `DASHBOARD`/`DB_PATH`/`LOG_DIR`
  from `spec.yaml` for a stateless cache-only sidecar.
