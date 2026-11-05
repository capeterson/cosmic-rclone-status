name := 'cosmic-rclone-ui'
appid := 'com.ca-peterson.CosmicRcloneUi'

default: build-release

build-release:
    cargo build --release

check:
    cargo clippy --all-targets --locked -- -D warnings

test:
    cargo test --locked

# Everything CI runs
ci:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked

# Print one status snapshot without the panel
status:
    cargo run --release -- --status

# Install under ~/.local so the applet can be added in COSMIC panel settings
install-user: build-release
    install -Dm0755 target/release/{{name}} ~/.local/bin/{{name}}
    install -Dm0644 resources/{{appid}}.desktop ~/.local/share/applications/{{appid}}.desktop
    install -Dm0644 resources/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg ~/.local/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg

uninstall-user:
    if [ -x ~/.local/bin/{{name}} ]; then ~/.local/bin/{{name}} --offline-remove; fi
    rm -f ~/.local/bin/{{name}} ~/.local/share/applications/{{appid}}.desktop ~/.local/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg

# Tests that run against the installed rclone, to catch behaviour that changes between its
# releases (start a throwaway `rclone rcd` for them)
test-live:
    #!/usr/bin/env bash
    set -euo pipefail
    dir=$(mktemp -d /tmp/rcs-it.XXXXXX)
    rclone rcd --rc-no-auth --rc-addr 127.0.0.1:5611 --config /dev/null --log-level NOTICE >"$dir/rcd.log" 2>&1 &
    pid=$!
    trap 'kill $pid 2>/dev/null; rm -rf "$dir"' EXIT
    sleep 2
    RCS_IT_DIR="$dir" cargo test --locked -- --ignored real_rclone

devid := 'com.ca-peterson.CosmicRcloneUiDev'

# Install a side-by-side dev copy ("rclone UI (dev)") that doesn't touch the real applet
install-dev:
    APP_ID_OVERRIDE={{devid}} cargo build --release
    install -Dm0755 target/release/{{name}} ~/.local/bin/{{name}}-dev
    sed -e 's/^Name=.*/Name=rclone UI (dev)/' -e 's/^Exec=.*/Exec={{name}}-dev/' resources/{{appid}}.desktop > /tmp/{{devid}}.desktop
    install -Dm0644 /tmp/{{devid}}.desktop ~/.local/share/applications/{{devid}}.desktop
    rm -f /tmp/{{devid}}.desktop
    cargo build --release  # restore the normal build in target/

uninstall-dev:
    if [ -x ~/.local/bin/{{name}}-dev ]; then ~/.local/bin/{{name}}-dev --offline-remove; fi
    rm -f ~/.local/bin/{{name}}-dev ~/.local/share/applications/{{devid}}.desktop
