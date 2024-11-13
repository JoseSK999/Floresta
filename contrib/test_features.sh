#!/bin/sh

# Exit immediately if any command fails
set -e

# Install or update cargo-hack (will skip if up-to-date)
cargo install cargo-hack --locked

crates="\
    floresta-chain \
    floresta-cli \
    floresta-common \
    floresta-compact-filters \
    floresta-electrum \
    floresta-watch-only \
    floresta-wire \
    floresta \
    florestad"

for crate in $crates; do
    # Determine the path to the crate
    if [ "$crate" = "florestad" ]; then
        path="$crate"
    else
        path="crates/$crate"
    fi

    # The default feature, if not used to conditionally compile code, can be skipped as the combinations already
    # include that case (see https://github.com/taiki-e/cargo-hack/issues/155#issuecomment-2474330839)
    if [ "$crate" = "floresta-compact-filters" ] || [ "$crate" = "floresta-electrum" ]; then
        # These two crates don't have a default feature
        skip_default=""
    else
        skip_default="--skip default"
    fi

    # Navigate to the crate's directory
    cd "$path" || exit 1
    echo "Testing all feature combinations for $crate..."
    export RUSTFLAGS="-Awarnings"

    # Test all feature combinations
    # shellcheck disable=SC2086
    cargo hack test --release --feature-powerset $skip_default --quiet
    cd - > /dev/null || exit 1
done
