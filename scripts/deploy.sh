#!/usr/bin/env bash
# Deploy the Open CAD Studio web (wasm) build to a Debian/Ubuntu server behind
# nginx with a Let's Encrypt certificate.
#
#   ssh -p 2222 admin9149@ntrip.host
#   git clone https://github.com/eugeny32/OCS.git && cd OCS     # or scp the repo
#   nohup bash scripts/deploy.sh > deploy.log 2>&1 &            # build takes a while
#   tail -f deploy.log
#
# Re-running the script updates the checkout, rebuilds and redeploys.
#
# Environment overrides:
#   DOMAIN         site name                      (default: cad.ntrip.host)
#   REPO_URL       repo to clone when the script is not run from a checkout
#   BRANCH         branch to build when cloning   (default: main)
#   CERTBOT_EMAIL  contact address for Let's Encrypt (optional)
#   SKIP_TLS=1     configure plain HTTP only
set -euo pipefail

DOMAIN="${DOMAIN:-cad.ntrip.host}"
REPO_URL="${REPO_URL:-https://github.com/eugeny32/OCS.git}"
BRANCH="${BRANCH:-main}"
CERTBOT_EMAIL="${CERTBOT_EMAIL:-}"
SKIP_TLS="${SKIP_TLS:-0}"
WEB_ROOT="/var/www/${DOMAIN}"
SRC_DIR_DEFAULT="${HOME}/ocs-src"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mwarning: %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31merror: %s\033[0m\n' "$*" >&2; exit 1; }

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  command -v sudo >/dev/null || die "sudo is required when not running as root"
  SUDO="sudo"
  sudo -v || die "sudo access is required"
fi

# --- source checkout -------------------------------------------------------
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "${script_dir}/../Trunk.toml" ]; then
  SRC_DIR="$(cd "${script_dir}/.." && pwd)"
else
  SRC_DIR="${SRC_DIR_DEFAULT}"
fi

# --- system packages -------------------------------------------------------
log "Installing system packages"
export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update -y
$SUDO apt-get install -y --no-install-recommends \
  build-essential pkg-config libssl-dev ca-certificates curl git xz-utils \
  python3 rsync nginx certbot python3-certbot-nginx dnsutils

# --- swap: the release wasm link needs several GB of RAM -------------------
mem_kb="$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
swap_kb="$(awk '/SwapTotal/ {print $2}' /proc/meminfo)"
if [ $((mem_kb + swap_kb)) -lt $((6 * 1024 * 1024)) ] && [ ! -e /swapfile ]; then
  log "Low memory ($((mem_kb / 1024)) MB RAM): adding a 4 GB swap file"
  $SUDO fallocate -l 4G /swapfile || $SUDO dd if=/dev/zero of=/swapfile bs=1M count=4096
  $SUDO chmod 600 /swapfile
  $SUDO mkswap /swapfile
  $SUDO swapon /swapfile
  grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' | $SUDO tee -a /etc/fstab >/dev/null
fi

# --- Rust toolchain --------------------------------------------------------
log "Installing Rust (stable) and the wasm32 target"
if ! command -v rustup >/dev/null && [ ! -x "${HOME}/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
. "${HOME}/.cargo/env"
rustup toolchain install stable --profile minimal
rustup default stable
rustup target add wasm32-unknown-unknown

# --- trunk and wasm-bindgen ------------------------------------------------
case "$(uname -m)" in
  x86_64)  arch="x86_64" ;;
  aarch64) arch="aarch64" ;;
  *) die "unsupported CPU architecture: $(uname -m)" ;;
esac

fetch_source() {
  if [ -d "${SRC_DIR}/.git" ]; then
    log "Updating ${SRC_DIR}"
    if [ "${SRC_DIR}" = "${SRC_DIR_DEFAULT}" ]; then
      git -C "${SRC_DIR}" fetch --depth 1 origin "${BRANCH}"
      git -C "${SRC_DIR}" checkout -B "${BRANCH}" FETCH_HEAD
    fi
  else
    log "Cloning ${REPO_URL} (${BRANCH})"
    git clone --depth 1 --branch "${BRANCH}" "${REPO_URL}" "${SRC_DIR}"
  fi
}
fetch_source
cd "${SRC_DIR}"

# The worker hook calls `wasm-bindgen` directly, so the CLI version must match
# the wasm-bindgen crate pinned in Cargo.lock.
wb_version="$(sed -n '/name = "wasm-bindgen"/{n;s/version = "\(.*\)"/\1/p;q;}' Cargo.lock)"
[ -n "${wb_version}" ] || die "cannot read the wasm-bindgen version from Cargo.lock"

mkdir -p "${HOME}/.cargo/bin"
if ! command -v wasm-bindgen >/dev/null || ! wasm-bindgen --version | grep -q "${wb_version}"; then
  log "Installing wasm-bindgen ${wb_version}"
  tmp="$(mktemp -d)"
  url="https://github.com/rustwasm/wasm-bindgen/releases/download/${wb_version}/wasm-bindgen-${wb_version}-${arch}-unknown-linux-musl.tar.gz"
  if curl -fsSL "${url}" | tar -xz -C "${tmp}" --strip-components=1; then
    install -m 0755 "${tmp}/wasm-bindgen" "${HOME}/.cargo/bin/wasm-bindgen"
  else
    warn "prebuilt wasm-bindgen not available, building from source (slow)"
    cargo install --locked wasm-bindgen-cli --version "${wb_version}"
  fi
  rm -rf "${tmp}"
fi

if ! command -v trunk >/dev/null; then
  log "Installing trunk"
  tmp="$(mktemp -d)"
  url="https://github.com/trunk-rs/trunk/releases/latest/download/trunk-${arch}-unknown-linux-gnu.tar.gz"
  if curl -fsSL "${url}" | tar -xz -C "${tmp}"; then
    install -m 0755 "${tmp}/trunk" "${HOME}/.cargo/bin/trunk"
  else
    warn "prebuilt trunk not available, building from source (slow)"
    cargo install --locked trunk
  fi
  rm -rf "${tmp}"
fi
wasm-bindgen --version
trunk --version

# --- build -----------------------------------------------------------------
log "Building the web app (this takes a while)"
rm -rf dist/app
trunk build --locked --release --public-url / --dist dist/app --html-output index.html web-app.html
[ -f dist/app/index.html ] || die "build did not produce dist/app/index.html"
printf '[]\n' > dist/app/supporters.json   # optional list, not available offline

# --- publish ---------------------------------------------------------------
log "Publishing to ${WEB_ROOT}"
$SUDO mkdir -p "${WEB_ROOT}"
$SUDO rsync -a --delete dist/app/ "${WEB_ROOT}/" 2>/dev/null || {
  $SUDO rm -rf "${WEB_ROOT:?}/"*
  $SUDO cp -a dist/app/. "${WEB_ROOT}/"
}
$SUDO chown -R www-data:www-data "${WEB_ROOT}"

# --- nginx -----------------------------------------------------------------
log "Configuring nginx for ${DOMAIN}"
conf="/etc/nginx/sites-available/${DOMAIN}"
if [ ! -e "${conf}" ] || ! grep -q 'listen 443' "${conf}"; then
  # Only (re)write the config before certbot has added its TLS block.
  $SUDO tee "${conf}" >/dev/null <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name ${DOMAIN};

    root ${WEB_ROOT};
    index index.html;

    gzip on;
    gzip_types text/css application/javascript application/json application/wasm image/svg+xml;
    gzip_min_length 1024;

    types { application/wasm wasm; }

    location / {
        try_files \$uri \$uri/ /index.html;
    }

    # Hashed trunk assets can be cached for a long time.
    location ~* \.(wasm|js|css|woff2?|ttf|svg|png|jpg|ico)\$ {
        expires 7d;
        add_header Cache-Control "public";
    }
    location = /index.html {
        add_header Cache-Control "no-cache";
    }
}
EOF
fi
$SUDO ln -sf "${conf}" "/etc/nginx/sites-enabled/${DOMAIN}"
$SUDO nginx -t
$SUDO systemctl enable --now nginx
$SUDO systemctl reload nginx

if command -v ufw >/dev/null && $SUDO ufw status | grep -q 'Status: active'; then
  log "Opening ports 80 and 443 in ufw"
  $SUDO ufw allow 'Nginx Full'
fi

# --- TLS -------------------------------------------------------------------
if [ "${SKIP_TLS}" = "1" ]; then
  warn "SKIP_TLS=1: leaving the site on plain HTTP"
else
  server_ip="$(curl -fsS -4 https://ifconfig.me 2>/dev/null || true)"
  domain_ip="$(dig +short A "${DOMAIN}" | tail -n1)"
  if [ -z "${domain_ip}" ]; then
    warn "${DOMAIN} has no A record yet. Point it at ${server_ip:-this server} and re-run the script (or run: sudo certbot --nginx -d ${DOMAIN})."
  elif [ -n "${server_ip}" ] && [ "${domain_ip}" != "${server_ip}" ]; then
    warn "${DOMAIN} resolves to ${domain_ip}, but this server is ${server_ip}. Fix the A record and re-run."
  else
    log "Requesting a Let's Encrypt certificate for ${DOMAIN}"
    if [ -n "${CERTBOT_EMAIL}" ]; then
      email_args=(-m "${CERTBOT_EMAIL}")
    else
      email_args=(--register-unsafely-without-email)
    fi
    $SUDO certbot --nginx -d "${DOMAIN}" --non-interactive --agree-tos --redirect "${email_args[@]}"
  fi
fi

log "Done"
echo "Site: http://${DOMAIN}/  (https://${DOMAIN}/ once the certificate is issued)"
