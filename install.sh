#!/usr/bin/env bash
# suckless-mcp installer/uninstaller
# Usage: ./install.sh [--uninstall]

set -euo pipefail

REPO_URL="https://github.com/roverbird/suckless-mcp.git"
REPO_DIR="/tmp/suckless-mcp-repo"
BINARY_NAME="suckless-mcp"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/suckless-mcp"
SKILLS_DIR="/opt/skills"
SERVICE_NAME="suckless-mcp"

# Direct download URL for pre-compiled binary (release 0.1)
BINARY_URL="https://github.com/roverbird/suckless-mcp/releases/download/0.3/suckless-mcp-x86_64-unknown-linux-gnu"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

uninstall() {
    info "Uninstalling suckless-mcp..."

    if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
        sudo systemctl stop "$SERVICE_NAME"
        sudo systemctl disable "$SERVICE_NAME"
    fi

    sudo rm -f "/etc/systemd/system/${SERVICE_NAME}.service"
    sudo systemctl daemon-reload
    sudo rm -f "${INSTALL_DIR}/${BINARY_NAME}"

    read -p "Remove config directory (${CONFIG_DIR})? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        sudo rm -rf "$CONFIG_DIR"
        info "Removed $CONFIG_DIR"
    fi

    read -p "Remove skills directory (${SKILLS_DIR})? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        sudo rm -rf "$SKILLS_DIR"
        info "Removed $SKILLS_DIR"
    fi

    if id "suckless" &>/dev/null; then
        read -p "Remove 'suckless' system user? [y/N] " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            sudo userdel -r suckless 2>/dev/null || warn "Could not remove user"
        fi
    fi

    rm -rf "$REPO_DIR"
    info "Uninstall complete"
    exit 0
}

install() {
    info "Installing suckless-mcp..."

    if [[ $EUID -eq 0 ]]; then
        error "Do not run as root. Use sudo when prompted."
    fi

    info "Fetching skills from repository..."
    if [[ -d "$REPO_DIR" ]]; then
        (cd "$REPO_DIR" && git pull)
    else
        git clone --depth 1 "$REPO_URL" "$REPO_DIR"
    fi

    sudo mkdir -p "$CONFIG_DIR" "$SKILLS_DIR"
    sudo mkdir -p /var/log/suckless-mcp

    # Download binary from official release
    info "Downloading binary from: $BINARY_URL"
    TMP_FILE=$(mktemp)
    if command -v curl &>/dev/null; then
        curl -L -o "$TMP_FILE" "$BINARY_URL"
    elif command -v wget &>/dev/null; then
        wget -O "$TMP_FILE" "$BINARY_URL"
    else
        error "curl or wget required"
    fi

    # Verify binary
    if ! file "$TMP_FILE" | grep -q "ELF.*x86-64"; then
        error "Downloaded file is not a valid Linux x86_64 binary"
    fi

    sudo mv "$TMP_FILE" "${INSTALL_DIR}/${BINARY_NAME}"
    sudo chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

    # Copy skills from repository
    info "Copying skills from repository to ${SKILLS_DIR}..."
    for skill_dir in "${REPO_DIR}/skills"/*/; do
        if [[ -d "$skill_dir" ]]; then
            skill_name=$(basename "$skill_dir")
            target_dir="${SKILLS_DIR}/${skill_name}"
            
            if [[ -d "$target_dir" ]]; then
                warn "Skill '${skill_name}' already exists, skipping..."
            else
                sudo cp -r "$skill_dir" "$target_dir"
                sudo find "$target_dir" -name "*.py" -exec chmod +x {} \;
                info "Installed skill: ${skill_name}"
            fi
        fi
    done

    # Create default config
    if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
        sudo tee "${CONFIG_DIR}/config.toml" > /dev/null << 'EOF'
listen_host = "127.0.0.1"
listen_port = 8080
max_concurrent_tools = 5
EOF
        info "Created default config: ${CONFIG_DIR}/config.toml"
    fi

    # Create system user
    if ! id "suckless" &>/dev/null; then
        sudo useradd -r -s /bin/false suckless
        info "Created system user: suckless"
    fi

    # Generate API key and create keys.toml directly (bypassing --keys-add bug)
    API_KEY=$(openssl rand -hex 32 2>/dev/null || echo "change-this-key-please")
    
    # Create keys.toml directly with sudo
    sudo tee "${CONFIG_DIR}/keys.toml" > /dev/null << EOF
[[keys]]
id = "admin"
key = "$API_KEY"
active = true
EOF
    info "Created keys.toml with API key: $API_KEY"
    
    # Set ownership after creating files
    sudo chown -R suckless:suckless "$CONFIG_DIR" "$SKILLS_DIR" /var/log/suckless-mcp

    # Create systemd service
    sudo tee "/etc/systemd/system/${SERVICE_NAME}.service" > /dev/null << EOF
[Unit]
Description=Suckless MCP Gateway
After=network.target

[Service]
Type=simple
User=suckless
Group=suckless
ExecStart=${INSTALL_DIR}/${BINARY_NAME} --serve
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info
StandardOutput=journal
StandardError=journal
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=${CONFIG_DIR} ${SKILLS_DIR} /var/log/suckless-mcp

[Install]
WantedBy=multi-user.target
EOF

    sudo systemctl daemon-reload
    sudo systemctl enable "$SERVICE_NAME"
    sudo systemctl start "$SERVICE_NAME"

    sleep 2
    if sudo systemctl is-active --quiet "$SERVICE_NAME"; then
        info "Service running ✓"
    else
        warn "Service not running. Check: sudo journalctl -u $SERVICE_NAME"
    fi

    echo
    info "Installation complete!"
    echo "Commands:"
    echo "  ${BINARY_NAME} --status"
    echo "  ${BINARY_NAME} --skills"
    echo "  sudo systemctl status $SERVICE_NAME"
    echo
    echo "Config:  $CONFIG_DIR/config.toml"
    echo "Keys:    $CONFIG_DIR/keys.toml"
    echo "API Key: $API_KEY"
    echo "Skills:  $SKILLS_DIR"
    echo
    echo "Next steps:"
    echo "1. Test: curl -H 'Authorization: Bearer $API_KEY' http://127.0.0.1:8080/mcp -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}' -H 'Content-Type: application/json'"
    echo "2. Set up Caddy reverse proxy"
}

if [[ "${1:-}" == "--uninstall" ]]; then
    uninstall
else
    install
fi
