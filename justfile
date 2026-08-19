set shell := ["bash", "-euo", "pipefail", "-c"]

# Build and sign the native macOS application bundle.
app:
    nix develop --command scripts/package-macos.sh
