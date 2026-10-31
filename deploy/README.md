# Deployment

The runner runs on the VPS (Ubuntu 24.04, Docker already installed) behind Caddy at
`https://run.henrikhuth.de`. Programs run in containers under gVisor (`runsc`).

| File | Installed as |
|---|---|
| `runner.toml` | `/etc/playground-runner/runner.toml` |
| `playground-runner.service` | `/etc/systemd/system/` |
| `playground-pull.service`, `playground-pull.timer` | `/etc/systemd/system/` |
| `Caddyfile` | `/etc/caddy/Caddyfile` |
| `update-runner.sh` | `/usr/local/sbin/update-playground-runner` |

The runner binary comes from the rolling `latest` release, built by `.github/workflows/release.yml`
on every push to main. Language images come from GHCR; the pull timer fetches `:playground` every
5 minutes and the runner switches to a new image within a minute.

## Before starting

- Push this repo's main branch so the release workflow has published a binary.
- Make the `luga` and `fluke` packages on GHCR public (*github.com/bolli24 → Packages → luga →
  Package settings → Change visibility*, same for fluke). Otherwise the VPS needs
  `docker login ghcr.io` with a token that has `read:packages`.
- Create a DNS record `run.henrikhuth.de` → `169.58.192.5` in Cloudflare. Start with
  **DNS only** (grey cloud) so Caddy can get its certificate directly; switch to **Proxied** with
  SSL/TLS mode **Full (strict)** once it works.

## Setup

All commands run as root on the VPS, from a checkout or copy of this `deploy/` directory.

### 1. gVisor

```sh
curl -fsSL https://gvisor.dev/archive.key | gpg --dearmor -o /usr/share/keyrings/gvisor-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main" \
  > /etc/apt/sources.list.d/gvisor.list
apt-get update && apt-get install -y runsc
runsc install
# Runtimes can be reloaded without restarting Docker, so other containers keep running
systemctl reload docker
docker run --rm --runtime=runsc hello-world
```

### 2. Runner

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin playground-runner
install -d /etc/playground-runner
install -m 644 runner.toml /etc/playground-runner/runner.toml
install -m 755 update-runner.sh /usr/local/sbin/update-playground-runner
install -m 644 playground-runner.service playground-pull.service playground-pull.timer /etc/systemd/system/
systemctl daemon-reload

systemctl start playground-pull   # pulls every language's image
/usr/local/sbin/update-playground-runner   # downloads the binary and (re)starts the service
systemctl enable playground-runner
systemctl enable --now playground-pull.timer

curl -s localhost:3100/v1/languages
```

### 3. Caddy

```sh
apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
apt-get update && apt-get install -y caddy
install -m 644 Caddyfile /etc/caddy/Caddyfile
systemctl reload caddy

curl -s https://run.henrikhuth.de/healthz
```

### 4. Firewall (recommended)

`ufw` is inactive on the VPS. Allow SSH first so enabling it can't lock you out, and keep Tailscale
traffic working:

```sh
ufw allow 22/tcp
ufw allow 80/tcp
ufw allow 443/tcp
ufw allow in on tailscale0
ufw enable
```

Docker publishes container ports past ufw, so check that every published port is bound to
`127.0.0.1` (`docker ps`).

## Operations

- Logs: `journalctl -u playground-runner -f`
- Update the runner after a push to main: `update-playground-runner`
- Pull images now instead of waiting for the timer: `systemctl start playground-pull`
- Check the deployed luga image against the contract (`-g docker` because the service user only
  gets the `docker` group through the unit's `SupplementaryGroups`):
  `cd /tmp && sudo -u playground-runner -g docker playground-runner --config /etc/playground-runner/runner.toml contract --lang luga`
