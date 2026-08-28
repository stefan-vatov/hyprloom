# Maintainer: Thethracian
pkgname=hyprloom
pkgver=0.3.9
pkgrel=1
pkgdesc="Save, restore, and reconcile Hyprland window sessions"
arch=('x86_64')
url="https://github.com/thethracian/hyprloom"
license=('MIT')
depends=('hyprland')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/releases/download/v$pkgver/$pkgname-$pkgver.tar.gz")
_source_digest='403b48b52ec11264b69f01f7e32fe1a3a56cb9b0a95d6c52bb9a7e2061d6bc4d'
sha256sums=("$_source_digest")
provides=('hyprflow')
conflicts=('hyprflow')
replaces=('hyprflow')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm0755 -t "$pkgdir/usr/bin/" "target/release/$pkgname"
    ln -s hyprloom "$pkgdir/usr/bin/hyprflow"
    install -Dm0644 /dev/null "$pkgdir/usr/share/$pkgname/source-digest"
    printf '%s\n' "$_source_digest" > "$pkgdir/usr/share/$pkgname/source-digest"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
