#!/usr/bin/env bash
set -Eeuo pipefail

packaging_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_root=$(cd -- "$packaging_dir/.." && pwd)
cd "$project_root"

package_id=$(cargo pkgid --package dogi)
version=${package_id##*#}
version=${version##*@}
release_tag=${DOGI_RELEASE_TAG:-}

if [[ -n "$release_tag" && "$release_tag" != "$version" ]]; then
    echo "release tag $release_tag does not match Dogi version $version" >&2
    exit 1
fi

metadata_version=$(
    sed -n 's/.*<release version="\([^"]*\)".*/\1/p' \
        packaging/linux/io.github.oksyd.dogi.metainfo.xml \
        | head -n 1
)
if [[ "$metadata_version" != "$version" ]]; then
    echo "AppStream release $metadata_version does not match Dogi version $version" >&2
    exit 1
fi

host_target=$(rustc -vV | sed -n 's/^host: //p')
case "$host_target" in
    x86_64-unknown-linux-gnu)
        debian_architecture=amd64
        ;;
    aarch64-unknown-linux-gnu)
        debian_architecture=arm64
        ;;
    *)
        echo "unsupported release target: $host_target" >&2
        exit 1
        ;;
esac

target_dir=${CARGO_TARGET_DIR:-$project_root/target}
if [[ "$target_dir" != /* ]]; then
    target_dir="$project_root/$target_dir"
fi
release_binary="$target_dir/release/dogi"
dist_dir=${DOGI_DIST_DIR:-$project_root/dist}

case "$dist_dir" in
    "$project_root"/*) ;;
    *)
        echo "DOGI_DIST_DIR must be inside the project workspace" >&2
        exit 1
        ;;
esac

if [[ -e "$dist_dir" ]]; then
    echo "release output already exists: $dist_dir" >&2
    exit 1
fi

work_dir=$(mktemp -d -t dogi-release.XXXXXXXXXX)
cleanup() {
    rm -rf -- "$work_dir"
}
trap cleanup EXIT

cargo build --release --locked --package dogi
mkdir -p "$dist_dir"

package_root="$work_dir/package-root"
while read -r source destination remainder; do
    [[ -z "$source" || "$source" == \#* ]] && continue
    if [[ -n "${remainder:-}" ]]; then
        echo "invalid Debian install manifest entry: $source $destination $remainder" >&2
        exit 1
    fi

    source_path="$project_root/$source"
    mode=0644
    if [[ "$source" == "target/release/dogi" ]]; then
        source_path="$release_binary"
        mode=0755
    fi
    if [[ ! -f "$source_path" ]]; then
        echo "missing release asset: $source_path" >&2
        exit 1
    fi

    install -D -m "$mode" \
        "$source_path" \
        "$package_root/$destination/$(basename -- "$source")"
done < "$project_root/packaging/deb/dogi.install"

strip --strip-unneeded "$package_root/usr/bin/dogi"
install -D -m 0644 \
    "$project_root/packaging/deb/copyright" \
    "$package_root/usr/share/doc/dogi/copyright"
mkdir -p "$package_root/DEBIAN"
install -m 0755 "$project_root/packaging/deb/postinst" "$package_root/DEBIAN/postinst"
install -m 0755 "$project_root/packaging/deb/postrm" "$package_root/DEBIAN/postrm"

installed_size=$(du -sk "$package_root" | cut -f1)
temporary_control="$work_dir/debian/control"
mkdir -p "$(dirname -- "$temporary_control")"
{
    printf '%s\n' \
        'Source: dogi' \
        'Section: utils' \
        'Priority: optional' \
        'Maintainer: oksyd <oksyd@users.noreply.github.com>' \
        'Standards-Version: 4.7.2' \
        ''
    sed \
        -e "s|@VERSION@|$version|g" \
        -e "s|@ARCHITECTURE@|$debian_architecture|g" \
        -e "s|@INSTALLED_SIZE@|$installed_size|g" \
        -e "s|@SHLIB_DEPENDS@|libc6|g" \
        "$project_root/packaging/deb/control.in"
} > "$temporary_control"

shlib_output=$(cd "$work_dir" && dpkg-shlibdeps -O "$package_root/usr/bin/dogi")
shlib_dependencies=${shlib_output#shlibs:Depends=}
if [[ -z "$shlib_dependencies" || "$shlib_dependencies" == "$shlib_output" ]]; then
    echo "dpkg-shlibdeps did not produce runtime dependencies" >&2
    exit 1
fi
escaped_dependencies=$(printf '%s' "$shlib_dependencies" | sed 's/[&|\\]/\\&/g')

sed \
    -e "s|@VERSION@|$version|g" \
    -e "s|@ARCHITECTURE@|$debian_architecture|g" \
    -e "s|@INSTALLED_SIZE@|$installed_size|g" \
    -e "s|@SHLIB_DEPENDS@|$escaped_dependencies|g" \
    "$project_root/packaging/deb/control.in" > "$package_root/DEBIAN/control"

deb_name="dogi_${version}_${debian_architecture}.deb"
dpkg-deb --root-owner-group --build "$package_root" "$dist_dir/$deb_name"

portable_name="dogi-${version}-${host_target}"
portable_root="$work_dir/$portable_name"
install -D -m 0755 "$package_root/usr/bin/dogi" "$portable_root/bin/dogi"
install -D -m 0644 "$project_root/LICENSE" "$portable_root/LICENSE"
install -D -m 0644 "$project_root/README.md" "$portable_root/README.md"
install -D -m 0644 "$project_root/CHANGELOG.md" "$portable_root/CHANGELOG.md"
install -D -m 0644 \
    "$project_root/packaging/linux/io.github.oksyd.dogi.desktop" \
    "$portable_root/share/applications/io.github.oksyd.dogi.desktop"
install -D -m 0644 \
    "$project_root/packaging/linux/io.github.oksyd.dogi.metainfo.xml" \
    "$portable_root/share/metainfo/io.github.oksyd.dogi.metainfo.xml"
install -D -m 0644 \
    "$project_root/packaging/linux/io.github.oksyd.dogi.svg" \
    "$portable_root/share/icons/hicolor/scalable/apps/io.github.oksyd.dogi.svg"
install -D -m 0644 \
    "$project_root/crates/dogi/assets/linux/70-dogi-logitech.rules" \
    "$portable_root/lib/udev/rules.d/70-dogi-logitech.rules"

source_date_epoch=${SOURCE_DATE_EPOCH:-}
if [[ -z "$source_date_epoch" ]]; then
    source_date_epoch=$(git log -1 --format=%ct 2>/dev/null || date +%s)
fi
tar \
    --sort=name \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --zstd \
    -C "$work_dir" \
    -cf "$dist_dir/$portable_name.tar.zst" \
    "$portable_name"

printf 'Created release assets in %s\n' "$dist_dir"
find "$dist_dir" -maxdepth 1 -type f -printf '%f\n' | sort
