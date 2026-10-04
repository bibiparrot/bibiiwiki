#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
package_version=$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' Cargo.toml | head -n 1)
version=${1:-$package_version}
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ || $version != "$package_version" ]]; then
  echo "Release version $version does not match Cargo.toml $package_version" >&2
  exit 1
fi

test -x target/release/bibiiwiki
mkdir -p dist/.work
arch=$(uname -m)

if [[ $(uname -s) == Darwin ]]; then
  case "$arch" in arm64|x86_64) ;; *) echo "Unsupported macOS architecture: $arch" >&2; exit 1 ;; esac
  app=dist/BIBIIWIKI.app
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" dist/.work/bibiiwiki.iconset
  cp target/release/bibiiwiki "$app/Contents/MacOS/bibiiwiki"
  cp LICENSE README.md bibiiwiki.example.yaml "$app/Contents/Resources/"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/bibi-icon.png --out "dist/.work/bibiiwiki.iconset/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" assets/bibi-icon.png --out "dist/.work/bibiiwiki.iconset/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns dist/.work/bibiiwiki.iconset -o "$app/Contents/Resources/bibiiwiki.icns"
  cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>bibiiwiki</string>
<key>CFBundleIdentifier</key><string>com.bibiparrot.bibiiwiki</string>
<key>CFBundleName</key><string>BIBIIWIKI</string>
<key>CFBundleDisplayName</key><string>BIBIIWIKI</string>
<key>CFBundleIconFile</key><string>bibiiwiki.icns</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
  plutil -lint "$app/Contents/Info.plist"
  codesign --force --sign - "$app"
  codesign --verify --deep --strict "$app"
  base="dist/bibiiwiki-$version-$arch-macOS"
  ditto -c -k --keepParent "$app" "$base.zip"
  pkgbuild --component "$app" --install-location /Applications "$base.pkg"
  hdiutil create -volname BIBIIWIKI -srcfolder "$app" -ov -format UDZO "$base.dmg"
elif [[ $(uname -s) == Linux ]]; then
  case "$arch" in aarch64|x86_64) ;; *) echo "Unsupported Linux architecture: $arch" >&2; exit 1 ;; esac
  base="bibiiwiki-$version-linux-$arch"
  stage="dist/.work/$base"
  mkdir -p "$stage"
  cp target/release/bibiiwiki LICENSE README.md bibiiwiki.example.yaml "$stage/"
  cp assets/bibi-icon.png "$stage/bibiiwiki.png"
  cat > "$stage/bibiiwiki.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=BIBIIWIKI
Comment=Intelligent local Markdown wiki
Exec=bibiiwiki
Icon=bibiiwiki
Categories=Office;Utility;
Terminal=false
EOF
  ldd "$stage/bibiiwiki" > "$stage/runtime-linkage.txt"
  if grep -q 'not found' "$stage/runtime-linkage.txt"; then cat "$stage/runtime-linkage.txt"; exit 1; fi
  tar -czf "dist/$base.tar.gz" -C dist/.work "$base"

  top="$root/dist/.work/rpm"
  mkdir -p "$top/SOURCES" "$top/SPECS" "$top/BUILD" "$top/RPMS" "$top/SRPMS"
  cp target/release/bibiiwiki "$top/SOURCES/bibiiwiki"
  cp assets/bibi-icon.png "$top/SOURCES/bibiiwiki.png"
  cp LICENSE "$top/SOURCES/LICENSE"
  cp "$stage/bibiiwiki.desktop" "$top/SOURCES/bibiiwiki.desktop"
  cat > "$top/SPECS/bibiiwiki.spec" <<EOF
%global debug_package %{nil}
Name: bibiiwiki
Version: $version
Release: 1
Summary: Intelligent local Markdown wiki desktop application
License: GPL-3.0-only
%description
BIBIIWIKI is a local Markdown wiki desktop application.
%install
install -Dm755 %{_sourcedir}/bibiiwiki %{buildroot}/usr/bin/bibiiwiki
install -Dm644 %{_sourcedir}/bibiiwiki.desktop %{buildroot}/usr/share/applications/bibiiwiki.desktop
install -Dm644 %{_sourcedir}/bibiiwiki.png %{buildroot}/usr/share/icons/hicolor/256x256/apps/bibiiwiki.png
install -Dm644 %{_sourcedir}/LICENSE %{buildroot}/usr/share/licenses/bibiiwiki/LICENSE
%files
/usr/bin/bibiiwiki
/usr/share/applications/bibiiwiki.desktop
/usr/share/icons/hicolor/256x256/apps/bibiiwiki.png
%license /usr/share/licenses/bibiiwiki/LICENSE
EOF
  rpmbuild --define "_topdir $top" --target "$arch" -bb "$top/SPECS/bibiiwiki.spec"
  cp "$top"/RPMS/*/*.rpm "dist/$base.rpm"

  appdir=dist/.work/AppDir
  mkdir -p "$appdir/usr/bin" "$appdir/usr/share/licenses/bibiiwiki"
  cp target/release/bibiiwiki "$appdir/usr/bin/bibiiwiki"
  cp LICENSE "$appdir/usr/share/licenses/bibiiwiki/LICENSE"
  cp assets/bibi-icon.png "$appdir/bibiiwiki.png"
  cp "$stage/bibiiwiki.desktop" "$appdir/bibiiwiki.desktop"
  cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$here/usr/bin/bibiiwiki" "$@"
EOF
  chmod +x "$appdir/AppRun"
  curl --fail --location --retry 3 -o dist/.work/appimagetool "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
  chmod +x dist/.work/appimagetool
  ARCH="$arch" dist/.work/appimagetool --appimage-extract-and-run "$appdir" "dist/$base.AppImage"
else
  echo "Unsupported operating system: $(uname -s)" >&2
  exit 1
fi

for asset in dist/bibiiwiki-"$version"-*; do test -s "$asset"; done
echo "Packaged BIBIIWIKI $version for $(uname -s) $arch"
