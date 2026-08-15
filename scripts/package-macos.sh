#!/usr/bin/env bash

set -euo pipefail

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly script_directory
workspace_root="$(cd -- "${script_directory}/.." && pwd)"
readonly workspace_root
readonly target_directory="${CARGO_TARGET_DIR:-${workspace_root}/target/macos-bundle}"
readonly app_bundle="${target_directory}/release/bundle/osx/Memelith.app"
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

codesign_arguments=(--force --deep --sign "${codesign_identity}")
if [[ "${codesign_identity}" != "-" ]]; then
    codesign_arguments+=(--options runtime --timestamp)
fi
codesign "${codesign_arguments[@]}" "${app_bundle}"
codesign --verify --deep --strict --verbose=2 "${app_bundle}"

printf 'Packaged %s\n' "${app_bundle}"
