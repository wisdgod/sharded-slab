#!/bin/sh
# Usage: prepare.sh REPO name=ref [name=ref ...]
# Exports each git ref of REPO into variants/<name> and generates the
# Cargo.toml and src/variants.rs that link them all into one binary.
set -eu
repo=$1; shift
cd "$(dirname "$0")"
rm -rf variants && mkdir variants
deps=""; calls=""; n=0
for spec in "$@"; do
    name=${spec%%=*}; ref=${spec#*=}; n=$((n + 1))
    mkdir "variants/$name"
    git -C "$repo" archive "$ref" | tar -x -C "variants/$name"
    sed -i "0,/^version = .*/s//version = \"0.0.$n\"/" "variants/$name/Cargo.toml"
    deps="$deps$name = { package = \"sharded-slab\", path = \"variants/$name\" }
"
    calls="$calls    suite!(\"$name\", $name, &f);
"
done
cat > Cargo.toml <<TOML
[package]
name = "slabbench"
version = "0.0.0"
edition = "2024"

[dependencies]
$deps
[profile.release]
debug = true
codegen-units = 1
TOML
printf '{\n%s}\n' "$calls" > src/variants.rs
