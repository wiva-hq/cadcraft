#!/bin/sh
# Builds the engine for Forest as an npm package tarball (`cadcraft-wasm-<version>.tgz`): the WebAssembly, its
# wasm-bindgen glue for the web (`engine.js`, `engine.d.ts`) and a package.json. Needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli at the version Cargo.toml pins.
set -eu
cd "$(dirname "$0")"
VERSION="${1:?usage: forest_pkg.sh <version, such as 0.17.0-forest.1>}"
OUT="$(pwd)/pkg"
rm -rf "$OUT"
CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  cargo build -p cadcraft-wasm --release --target wasm32-unknown-unknown
wasm-bindgen ../../target/wasm32-unknown-unknown/release/cadcraft_wasm.wasm --target web --out-dir "$OUT" --out-name engine
cat > "$OUT/package.json" <<JSON
{
  "name": "cadcraft-wasm",
  "version": "$VERSION",
  "description": "CADCraft's engine for Forest, in WebAssembly",
  "license": "MIT OR Apache-2.0",
  "type": "module",
  "main": "engine.js",
  "types": "engine.d.ts",
  "files": ["engine.js", "engine.d.ts", "engine_bg.wasm", "engine_bg.wasm.d.ts"],
  "exports": { ".": "./engine.js", "./engine_bg.wasm": "./engine_bg.wasm" }
}
JSON
(cd "$OUT" && npm pack --silent)
ls -la "$OUT"/*.tgz
