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
source=("$pkgname-$pkgver.tar.gz::$url/releases/download/v$pkgver/$pkgname-$pkgver.tar.gz")
sha256sums=('2549618f6d6b4ea1578c2e9826f521b6c235102e8fefef24dd350993dd1841a4')
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
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
