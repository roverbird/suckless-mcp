## Minimal Dockerfile

```dockerfile
FROM alpine:latest

# Copy binary
COPY suckless-mcp /usr/local/bin/suckless-mcp
RUN chmod +x /usr/local/bin/suckless-mcp

# Create directories
RUN mkdir -p /etc/suckless-mcp /opt/skills

EXPOSE 8080

CMD ["suckless-mcp", "serve", "--config", "/etc/suckless-mcp/config.toml"]
```

## Build & Run

```bash
# Build image
docker build -t suckless-mcp .

# Run with config and skills mounted
docker run -p 8080:8080 \
  -v $(pwd)/config.toml:/etc/suckless-mcp/config.toml \
  -v $(pwd)/skills:/opt/skills \
  -v $(pwd)/keys.toml:/etc/suckless-mcp/keys.toml \
  suckless-mcp
```

## Docker Compose (with Caddy)

```yaml
version: '3.8'

services:
  suckless-mcp:
    image: suckless-mcp
    container_name: suckless-mcp
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"
    volumes:
      - ./config.toml:/etc/suckless-mcp/config.toml:ro
      - ./keys.toml:/etc/suckless-mcp/keys.toml:ro
      - ./skills:/opt/skills:ro
    environment:
      - RUST_LOG=info

  caddy:
    image: caddy:alpine
    container_name: caddy
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile
      - caddy_data:/data
    depends_on:
      - suckless-mcp

volumes:
  caddy_data:
```

## Multi-Arch Build (Linux x86_64 + ARM64)

```dockerfile
# Use distroless for even smaller image
FROM gcr.io/distroless/cc-debian12

COPY suckless-mcp /usr/local/bin/suckless-mcp

EXPOSE 8080

CMD ["/usr/local/bin/suckless-mcp", "serve", "--config", "/etc/suckless-mcp/config.toml"]
```

## One-liner run (no Dockerfile)

```bash
docker run --rm -p 8080:8080 \
  -v $(pwd)/config.toml:/config.toml \
  -v $(pwd)/skills:/skills \
  alpine:latest \
  /bin/sh -c "wget -O /tmp/suckless-mcp https://github.com/your/suckless-mcp/releases/latest/download/suckless-mcp-linux-x86_64 && \
              chmod +x /tmp/suckless-mcp && \
              /tmp/suckless-mcp serve --config /config.toml"
```

## Pre-built images (if you publish)

```bash
# Pull from registry
docker pull ghcr.io/yourusername/suckless-mcp:latest

# Run
docker run -p 8080:8080 \
  -v $(pwd)/config.toml:/config.toml \
  -v $(pwd)/skills:/skills \
  ghcr.io/yourusername/suckless-mcp
```

## Caddyfile for Docker setup

```Caddyfile
mcp.yourdomain.com {
    reverse_proxy suckless-mcp:8080
}
```

## Notes

- ✅ Works perfectly — stateless binary, no runtime dependencies
- ✅ Skills mounted as volume — add/remove tools without rebuild
- ✅ Config mounted — change settings without rebuild
- ✅ Alpine/distroless images are ~8-12MB total
- ✅ Same suckless philosophy: no database, no init system, no complexity

The binary runs exactly the same in Docker as on bare metal — because there's nothing else it needs.
