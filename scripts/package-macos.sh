#!/usr/bin/env bash

set -euo pipefail

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly script_directory
workspace_root="$(cd -- "${script_directory}/.." && pwd)"
readonly workspace_root
readonly target_directory="${CARGO_TARGET_DIR:-${workspace_root}/target/macos-bundle}"
readonly app_bundle="${target_directory}/release/bundle/osx/Memelith.app"
readonly app_executable="${app_bundle}/Contents/MacOS/memelith"
readonly codesign_identity="${CODESIGN_IDENTITY:--}"

cd "${workspace_root}"
export CARGO_TARGET_DIR="${target_directory}"
export MACOSX_DEPLOYMENT_TARGET="14.0"

cargo bundle --package memelith --bin memelith --release --format osx

(
    cd "${app_bundle}/Contents"
    dylibbundler \
        --fix-file "MacOS/memelith" \
        --bundle-deps \
        --dest-dir "Frameworks" \
        --install-path "@executable_path/../Frameworks/" \
        --create-dir \
        --overwrite-files \
        --no-codesign
)

# FFmpeg's Nix closure uses GNU libiconv, while the application itself uses
# Apple's ABI-compatible library with the same install name. dylibbundler
# cannot represent both under libiconv.2.dylib, so give GNU libiconv a unique
# install name and point the FFmpeg/IDN2 closure at it.
frameworks_directory="${app_bundle}/Contents/Frameworks"
readonly frameworks_directory
gnu_iconv_source="${GNU_LIBICONV_PATH:?GNU_LIBICONV_PATH must point to GNU libiconv}"
if ! nm -gU "${gnu_iconv_source}" 2>/dev/null | grep '_libiconv$' >/dev/null; then
    printf 'GNU_LIBICONV_PATH does not export _libiconv: %s\n' "${gnu_iconv_source}" >&2
    exit 1
fi
gnu_iconv_target="${frameworks_directory}/libgnuiconv.2.dylib"
cp -f "${gnu_iconv_source}" "${gnu_iconv_target}"
chmod +w "${gnu_iconv_target}"
install_name_tool -id "@executable_path/../Frameworks/libgnuiconv.2.dylib" "${gnu_iconv_target}"
for framework in "${frameworks_directory}"/*.dylib; do
    [[ "${framework}" == "${gnu_iconv_target}" ]] && continue
    if dyld_info -imports "${framework}" 2>/dev/null | grep ' _libiconv' >/dev/null; then
        install_name_tool -change \
            "@executable_path/../Frameworks/libiconv.2.dylib" \
            "@executable_path/../Frameworks/libgnuiconv.2.dylib" "${framework}"
    fi
done

codesign_arguments=(--force --deep --sign "${codesign_identity}")
if [[ "${codesign_identity}" != "-" ]]; then
    codesign_arguments+=(--options runtime --timestamp)
fi
codesign "${codesign_arguments[@]}" "${app_bundle}"
codesign --verify --deep --strict --verbose=2 "${app_bundle}"

# codesign validates signatures but does not resolve imported symbols. Launch
# briefly with isolated settings so dyld failures are caught during packaging.
smoke_home="$(mktemp -d "${TMPDIR:-/tmp}/memelith-package-smoke.XXXXXX")"
readonly smoke_home
smoke_log="${smoke_home}/memelith.log"
readonly smoke_log
smoke_pid=""
cleanup_smoke_test() {
    if [[ -n "${smoke_pid}" ]] && kill -0 "${smoke_pid}" 2>/dev/null; then
        kill "${smoke_pid}" 2>/dev/null || true
        wait "${smoke_pid}" 2>/dev/null || true
    fi
    rm -rf -- "${smoke_home}"
}
trap cleanup_smoke_test EXIT

HOME="${smoke_home}" XDG_CONFIG_HOME="${smoke_home}/config" \
    "${app_executable}" >"${smoke_log}" 2>&1 &
smoke_pid=$!
sleep 1
if ! kill -0 "${smoke_pid}" 2>/dev/null; then
    wait "${smoke_pid}" || smoke_status=$?
    printf 'Packaged application failed its launch check (status %s):\n' "${smoke_status:-0}" >&2
    cat "${smoke_log}" >&2
    exit 1
fi
kill "${smoke_pid}"
wait "${smoke_pid}" 2>/dev/null || true
smoke_pid=""

printf 'Packaged %s\n' "${app_bundle}"
