# za

## Install

```bash
curl -fsSL https://github.com/lvillis/za/releases/latest/download/za-linux-amd64 | sudo install -m 0755 /dev/stdin /usr/local/bin/za
```

## agent-browser

Install the native binary from [agent-browser releases](https://github.com/vercel-labs/agent-browser/releases) with SHA-256 verification:

```bash
za tool install agent-browser
za run agent-browser -- install  # Download Chrome on first use
za tool update agent-browser
```

Use `za tool install agent-browser@0.38.1` to pin a version. Supported platforms are Linux (x64/ARM64, GNU or musl), macOS (x64/ARM64), and Windows (x64).
