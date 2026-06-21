#!/usr/bin/env bash
set -euo pipefail

# ── 配置 ──────────────────────────────────────────────
RUNNER_USER="${RUNNER_USER:-github-runner}"
RUNNER_DIR="/home/${RUNNER_USER}/actions-runner"
REPO_URL="https://github.com/0x11-dev/serverless_db"
# 在 GitHub 仓库 Settings → Actions → Runners → New self-hosted runner 中获取 TOKEN
RUNNER_TOKEN="${RUNNER_TOKEN:?请设置 RUNNER_TOKEN 环境变量，从 GitHub 仓库 Settings → Actions → Runners 获取}"

# ── 1. 创建用户 ──────────────────────────────────────
if ! id "$RUNNER_USER" &>/dev/null; then
    useradd -m -s /bin/bash "$RUNNER_USER"
    echo "✅ 创建用户 $RUNNER_USER"
fi

# ── 2. 安装依赖 ──────────────────────────────────────
echo "📦 安装系统依赖..."
apt-get update -qq
apt-get install -y -qq \
    curl wget git build-essential pkg-config libssl-dev \
    ca-certificates docker.io docker-compose-plugin \
    >/dev/null

# 允许 runner 用户使用 docker
usermod -aG docker "$RUNNER_USER"
systemctl enable docker --now

# ── 3. 安装 Node.js 22 ───────────────────────────────
if ! command -v node &>/dev/null || [[ "$(node -v)" != v22* ]]; then
    echo "📦 安装 Node.js 22..."
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null
    apt-get install -y -qq nodejs >/dev/null
fi
echo "  Node.js: $(node -v)"

# ── 4. 安装 Rust ─────────────────────────────────────
sudo -u "$RUNNER_USER" bash -c '
if ! command -v cargo &>/dev/null; then
    echo "📦 安装 Rust..."
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
source "$HOME/.cargo/env"
echo "  Rust: $(rustc --version)"
'

# ── 5. 下载 GitHub Actions Runner ───────────────────
echo "📥 下载 GitHub Actions Runner..."
sudo -u "$RUNNER_USER" bash -c "
mkdir -p '$RUNNER_DIR'
cd '$RUNNER_DIR'

if [ ! -f config.sh ]; then
    # 获取最新 runner 版本
    RUNNER_VERSION=\$(curl -sL https://api.github.com/repos/actions/runner/releases/latest | grep 'tag_name' | cut -d'\"' -f4 | sed 's/v//')
    echo \"  Runner version: \$RUNNER_VERSION\"
    curl -sL \"https://github.com/actions/runner/releases/download/v\${RUNNER_VERSION}/actions-runner-linux-x64-\${RUNNER_VERSION}.tar.gz\" -o runner.tar.gz
    tar xzf runner.tar.gz
    rm runner.tar.gz
fi
"

# ── 6. 注册 Runner ───────────────────────────────────
echo "🔐 注册 Runner..."
sudo -u "$RUNNER_USER" bash -c "
cd '$RUNNER_DIR'
./config.sh --url '$REPO_URL' --token '$RUNNER_TOKEN' \
    --name 'serverless-db-runner' \
    --labels 'self-hosted,linux' \
    --unattended --replace
"

# ── 7. 安装并启动 systemd 服务 ───────────────────────
echo "🚀 安装 systemd 服务..."
cd "$RUNNER_DIR"
./svc.sh install "$RUNNER_USER"
./svc.sh start

echo ""
echo "✅ Runner 部署完成！"
echo "   状态: sudo -u $RUNNER_USER bash -c 'cd $RUNNER_DIR && ./svc.sh status'"
echo "   日志: journalctl -u actions.runner.0x11-dev-serverless_db.serverless-db-runner -f"
