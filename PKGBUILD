# Maintainer: Thethracian
pkgname=hyprloom
pkgver=0.3.0
pkgrel=1
pkgdesc="Save, restore, and reconcile Hyprland window sessions"
arch=('x86_64')
url="https://github.com/thethracian/hyprloom"
license=('MIT')
depends=('hyprland')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
sha256sums=('5c4d997898e7ac88fe7e8fbc50ce58d51e041d9f9bb691076f8e0dd09f88939d')
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
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
