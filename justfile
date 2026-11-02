name := 'cosmic-rclone-status'
appid := 'com.ca-peterson.CosmicRcloneStatus'

default: build-release

build-release:
    cargo build --release

check:
    cargo clippy -- -W clippy::pedantic

# Print one status snapshot without the panel
status:
    cargo run --release -- --status

# Install under ~/.local so the applet can be added in COSMIC panel settings
install-user: build-release
    install -Dm0755 target/release/{{name}} ~/.local/bin/{{name}}
    install -Dm0644 resources/{{appid}}.desktop ~/.local/share/applications/{{appid}}.desktop
    install -Dm0644 resources/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg ~/.local/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg

uninstall-user:
    rm -f ~/.local/bin/{{name}} ~/.local/share/applications/{{appid}}.desktop ~/.local/share/icons/hicolor/scalable/apps/{{appid}}-symbolic.svg
