# Maintainer: Thethracian
pkgname=hyprloom
pkgver=0.3.8
pkgrel=1
pkgdesc="Save, restore, and reconcile Hyprland window sessions"
arch=('x86_64')
url="https://github.com/thethracian/hyprloom"
license=('MIT')
depends=('hyprland')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/releases/download/v$pkgver/$pkgname-$pkgver.tar.gz")
_source_digest='96b4f66dbe5ce271ab16724c7f70f7c06ecafc3ac58b8f87df92f3041dc9d4e9'
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
