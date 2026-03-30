# Maintainer: Boo_mario <boo15mario.com>
pkgname=waytray
pkgver=1.0.1
pkgrel=3
pkgdesc="Wayland system tray with daemon/client architecture (forked, built with Rust 1.85)"
arch=('aarch64' 'x86_64')
url="https://github.com/Boo15mario/waytray"
license=('MIT')
depends=('gtk4' 'gstreamer' 'dbus')
makedepends=('rust' 'cargo')
optdepends=('pipewire-pulse: for audio volume control module'
            'power-profiles-daemon: for power profile switching')
source=("$pkgname-$pkgver.tar.gz::https://github.com/boo15mario/$pkgname/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('5a9e2652cb0ec252305c55087212500193466b94a1701710e751937b63b87223')

prepare() {
    cd "$pkgname-$pkgver"
    # Fix: ring 0.17 asm incompatibility with Rust 1.94+ lld linker
    # Replace rustls-tls with native-tls to avoid ring dependency (must be before generate-lockfile)
    sed -i 's/features = \["json", "rustls-tls"\]/features = ["json", "native-tls"]/' waytray-daemon/Cargo.toml
    # Generate Cargo.lock since it's not in the source tarball
    cargo generate-lockfile
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=1.85.0
    export CARGO_TARGET_DIR=target
    cargo build --release --locked --all-features
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=1.85.0
    cargo test --release --frozen --all-features
}

package() {
    cd "$pkgname-$pkgver"

    # Install binaries
    install -Dm755 target/release/waytray-daemon "$pkgdir/usr/bin/waytray-daemon"
    install -Dm755 target/release/waytray "$pkgdir/usr/bin/waytray"

    # Install license
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
