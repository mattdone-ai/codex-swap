# Codext managed-runtime companion patch

`codext-managed-auth.patch` adds the credential serialization contract required by `xswap session` to [Loongphy/codext](https://github.com/Loongphy/codext). It is based on tag `codext-v0.160.0-263fb4a`, commit `263fb4a9d7e9cf02ff7b782572d82134310c39fb`.

The patch SHA-256 is:

```text
6d530a3539b6dc2b7c62c0b912bf3e3e8d84db4cdca7f58809f8868114bea558  codext-managed-auth.patch
```

The accepted GNU package is version `0.160.0-xswap.6d530a3539b6`. Its `bin/codex` SHA-256 is `dd292100862c580d6af22106235a393185a91dd8bdf68dcdb768818f92c02306`; its `codex-package.json` SHA-256 is `fcf88b65069a52b84f429fba68b1fe22ddc29b923cb363ac546128487c1c987b`.

The patch makes a Codext process opt into managed behavior only when its `CODEX_HOME` contains a regular `.xswap-managed-runtime` file owned by the effective user, with mode `0600` and exact bytes `v1\n`. In that runtime, token refresh and persistence take an exclusive OS file lock on `auth.json.xswap.lock`. xswap takes the same lock while it captures and replaces runtime credentials. This keeps a refresh from persisting stale credentials over a completed rotation. A managed 401 reloads a newly rotated account and retries at the next safe boundary. An absent marker preserves Codext's existing behavior.

Managed mode is supported on Unix. A present managed marker fails closed on Windows; an unmarked Codext home keeps the stock behavior there. xswap's workflows outside this companion managed mode retain their existing Windows support.

## Reproduce the source

Use the exact base and Rust 1.95.0:

```sh
git clone https://github.com/Loongphy/codext.git codext-xswap
cd codext-xswap
git checkout 263fb4a9d7e9cf02ff7b782572d82134310c39fb
git apply --index /path/to/codex-swap/contrib/codext/codext-managed-auth.patch
cd codex-rs
rustup toolchain install 1.95.0
```

The build host had the Ubuntu runtime OpenSSL 3 libraries but no unversioned linker names. These commands recreate the pinned headers and local linker directory from Ubuntu 26.04 `resolute-updates`:

```sh
mkdir -p deps-xswap/libssl-dev-root deps-xswap/openssl-dynamic
curl -fL -o deps-xswap/libssl-dev_3.5.5-1ubuntu3.7_amd64.deb \
  http://au.archive.ubuntu.com/ubuntu/pool/main/o/openssl/libssl-dev_3.5.5-1ubuntu3.7_amd64.deb
printf '%s  %s\n' \
  d1cf6cafb141490b7020e392964b1f901835d60cb88c3a68ef0f95ef5e930de4 \
  deps-xswap/libssl-dev_3.5.5-1ubuntu3.7_amd64.deb | sha256sum -c -
dpkg-deb -x deps-xswap/libssl-dev_3.5.5-1ubuntu3.7_amd64.deb deps-xswap/libssl-dev-root
ln -s /usr/lib/x86_64-linux-gnu/libssl.so.3 deps-xswap/openssl-dynamic/libssl.so
ln -s /usr/lib/x86_64-linux-gnu/libcrypto.so.3 deps-xswap/openssl-dynamic/libcrypto.so
```

The validated host runtime files were `libssl.so.3` SHA-256 `63b47f444efa588e291e9b650a5ba2ab02f856356f2d50f13a2e477db9119e09` and `libcrypto.so.3` SHA-256 `5385f0436ac2e284ba8c5fb1488874057c29c04be72dea32add832694f381e28`. A different supported host may instead use its normal `libssl-dev` package and omit the two local linker symlinks.

From `codex-rs`, the gates use a private Cargo home and target directory, eight build jobs, the pinned OpenSSL headers and libraries, and disabled debug symbols:

```sh
export GIT_CONFIG_GLOBAL=/dev/null
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_HOME="$PWD/../.cargo-xswap"
export CARGO_TARGET_DIR="$PWD/../target-xswap"
export CARGO_BUILD_JOBS=8
export CARGO_PROFILE_DEV_DEBUG=0
export OPENSSL_INCLUDE_DIR="$PWD/../deps-xswap/libssl-dev-root/usr/include"
export OPENSSL_LIB_DIR="$PWD/../deps-xswap/openssl-dynamic"
export CFLAGS="-I$PWD/../deps-xswap/libssl-dev-root/usr/include/x86_64-linux-gnu"

cargo +1.95.0 check -p codex-login
cargo +1.95.0 clippy -p codex-login --lib -- -D warnings
cargo +1.95.0 test -p codex-login --test all suite::auth_refresh::xswap_ -- --nocapture
cargo +1.95.0 test -p codex-login --test all suite::auth_refresh -- --nocapture
cargo +1.95.0 test -p codex-tui usage_limit_recovery_ -- --nocapture

unset CARGO_PROFILE_DEV_DEBUG
export CARGO_PROFILE_RELEASE_DEBUG=0
export CODEX_BWRAP_SHA256=77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c
export STABLE_GIT_COMMIT=263fb4a9d7e9cf02ff7b782572d82134310c39fb
cargo +1.95.0 build --locked --release -p codex-cli --bin codex
```

Install the resulting binary as part of a complete Codext platform package. Keep the package metadata and helper files used by the TUI and its local tools; do not install the binary by itself. Give the packaged executable mode `0555`, put each build at a unique versioned path, and configure xswap with that absolute path and its SHA-256 digest. xswap verifies and holds the same executable inode through `exec` to close an atomic path-replacement race. xswap forces interactive, resume, and fork sessions to use the embedded patched backend, so Codext's shared-daemon updater cannot replace the pinned build. Foreground app-server mode remains available for clients that manage retries.

The accepted package preserved helpers from the upstream `codext-linux-x64-0.160.0-263fb4a.tar.gz` release archive, SHA-256 `a2b74d1d0442fd24ee09ce9d498274f28921289e235a569faa3b4df4d17b7457`. After extracting that immutable archive as `EXISTING`, assemble the package from the clone root:

```sh
PACKAGE=/path/to/codext-package-0.160.0-xswap.6d530a3539b6
EXISTING=/path/to/extracted/codext-0.160.0-263fb4a
CODEX_REPO_ROOT="$PWD" python3 scripts/build_codex_package.py \
  --target x86_64-unknown-linux-gnu \
  --variant codex \
  --package-version 0.160.0-xswap.6d530a3539b6 \
  --package-dir "$PACKAGE" \
  --entrypoint-bin "$PWD/target-xswap/release/codex" \
  --code-mode-host-bin "$EXISTING/bin/codex-code-mode-host" \
  --bwrap-bin "$EXISTING/codex-resources/bwrap" \
  --rg-bin "$EXISTING/codex-path/rg" \
  --zsh-bin "$EXISTING/codex-resources/zsh/bin/zsh" \
  --force
ln -s bin/codex "$PACKAGE/codext"
chmod 0555 "$PACKAGE/bin/codex"
chmod 0755 "$PACKAGE/bin/codex-code-mode-host" "$PACKAGE/codex-resources/bwrap" \
  "$PACKAGE/codex-path/rg" "$PACKAGE/codex-resources/zsh/bin/zsh"
```

## Validation

The final source passed these Rust 1.95.0 gates:

- `cargo check -p codex-login --tests`
- Clippy for `codex-login` with warnings denied
- 8 managed-runtime authentication and cross-process lock tests
- the full 29-test `auth_refresh` suite
- 2 TUI tests proving a changed identity dispatches exactly one parked continuation before queued follow-ups, while an unchanged identity stays parked
- optimized `codex-cli` release build

Release acceptance passed a scratch live test that resumed a conversation created by stock Codex, preserved the stock SQLite files, blocked rotation while Codext held the common auth lock, rotated to another real account, and continued the same thread in the same app-server process without an explicit reload call. A separate terminal smoke test reached the real TUI's ready composer through the descriptor-held executable, confirmed the embedded backend did not install a shared daemon, and exited cleanly.

The upstream `rate_limit_recovery_holds_submissions_until_model_change` test fails on the exact base because `set_model` leaves `suppress_queue_autosend` set. That path does not invoke this patch. The managed identity-change path clears the suppression flag and is covered by the two focused continuation tests above. The patch intentionally does not change the unrelated model-switch behavior.

Rust stable 1.99 passed the authentication suite, but the TUI test and release build did not complete because the unchanged `codex-chatgpt` crate exceeded the compiler query-depth limit. The reproducible TUI and release gates therefore pin Rust 1.95.0.

## License and attribution

Codext is distributed under the Apache License 2.0 and carries its own `LICENSE` and `NOTICE` files. This repository supplies a source patch and build instructions; a redistributed full package must retain the upstream license, notice, and package metadata.
