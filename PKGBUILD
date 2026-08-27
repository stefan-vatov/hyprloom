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
sha256sums=('3ac3eaa017b162b01729777539b48f1d0b678e76c2682194b24ae85b7d961453')
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
