# Maintainer: Shaun Lastra <shaun@lastra.us>
pkgname=rump
pkgver=0.1.0
pkgrel=1
pkgdesc='Icecast streaming client with GTK4 UI — spiritual successor to BUTT'
arch=('x86_64')
license=('MIT')
depends=('gtk4' 'libadwaita' 'libvorbis' 'libogg' 'pipewire' 'playerctl')
makedepends=('rust' 'cargo' 'pkg-config')

build() {
    cd "$startdir"
    cargo build --release
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/rump" "$pkgdir/usr/bin/rump"
    install -Dm644 "rump.desktop" "$pkgdir/usr/share/applications/rump.desktop"
}
