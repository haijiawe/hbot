#!/usr/bin/env bash
# HBot 的 Linux 安装脚本（草稿）。
#
# 状态：**从未在真机上跑过**。写它的时候手上没有 Linux 主机，所以它只能和
# packaging/README.md 的 §2 对应着看 —— 那一节是权威步骤，这个脚本只是把它串起来。
# 第一次用之前请通读，并在有快照的机器上试。
#
# 用法：
#   sudo ./packaging/install.sh [/path/to/hbot]
#
# 幂等：重复跑不会重复建用户、不会覆盖已有的 token 文件（token 一旦换掉，
# 现有客户端全部掉线，所以"已存在就不动"比"总是最新"更重要）。
set -euo pipefail

BIN_SOURCE="${1:-./hbot}"
BIN_TARGET=/usr/local/bin/hbot
UNIT_TARGET=/etc/systemd/system/hbot.service
LOGROTATE_TARGET=/etc/logrotate.d/hbot
ENV_DIR=/etc/hbot
ENV_FILE="$ENV_DIR/hbot.env"
STATE_DIR=/var/lib/hbot
SERVICE_USER=hbot

die() { echo "错误：$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "请用 root 跑（sudo ./packaging/install.sh）"
[[ -f $BIN_SOURCE ]] || die "找不到二进制：$BIN_SOURCE（先 cargo build --release -p hbot-cli，再指过来）"
command -v systemctl >/dev/null || die "这台机器没有 systemctl，本脚本不适用"

echo '==> 1/6 用户'
if id -u "$SERVICE_USER" >/dev/null 2>&1; then
    echo "    用户 $SERVICE_USER 已存在，跳过"
else
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
    echo "    已创建 $SERVICE_USER"
fi

echo '==> 2/6 二进制'
install -Dm755 "$BIN_SOURCE" "$BIN_TARGET"
echo "    $BIN_TARGET"

echo '==> 3/6 token'
install -d -m 750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$ENV_DIR"
if [[ -s $ENV_FILE ]]; then
    echo "    $ENV_FILE 已存在，保留原 token（要换就删掉它再跑一遍）"
else
    # token 由二进制自己生成：脚本不该知道它长什么样，也不该让它进 shell 历史。
    sudo -u "$SERVICE_USER" "$BIN_TARGET" token --write "$ENV_FILE"
    chmod 600 "$ENV_FILE"
    echo "    已写入 $ENV_FILE（0600）"
fi

echo '==> 4/6 systemd 与 logrotate'
install -Dm644 "$(dirname "$0")/systemd/hbot.service" "$UNIT_TARGET"
install -Dm644 "$(dirname "$0")/logrotate.d/hbot" "$LOGROTATE_TARGET"
echo "    $UNIT_TARGET / $LOGROTATE_TARGET"

echo '==> 5/6 数据目录'
# StateDirectory= 会替我们建它；这里只在已存在时纠正属主（不要手工 mkdir，
# 那会让 systemd 的 0750 与 SELinux 标签都对不上）。
if [[ -d $STATE_DIR ]]; then
    chown -R "$SERVICE_USER:$SERVICE_USER" "$STATE_DIR"
fi

echo '==> 6/6 启动'
systemctl daemon-reload
systemctl enable --now hbot

echo
echo '装好了。接下来：'
echo '  systemctl status hbot'
echo '  journalctl -u hbot -n 50 --no-pager     # 找那行 ready JSON'
echo '  curl -s http://127.0.0.1:8787/healthz   # 期望 {"status":"ok",...}'
echo
echo "客户端要用的 token 在 $ENV_FILE 里（HBOT_TOKEN=…）。"
