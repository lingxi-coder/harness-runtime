# Mbed TLS 3.6.7 native source review — 2026-11-03

Status: **partial Codex AI-assisted static review, not a complete audit or approval**. This note binds the original committed WIP `3c37ca847abaa92bd7d8a16df17beaa7d6924678`; it does not establish safety, device acceptance, or final build identity. After WIP, the separate stream-ownership fix was committed as `fb6cbef9790701717e05a5d2e81decb8f59e27f9`, and the build-environment fix and test as `0159f5553d4103e4ff2a9755a5f9fcc12f6ac5ef`. Subsequent stream edits may still be uncommitted. Do not infer these later identities from the WIP commit. No CA lifecycle implementation, CA probe, or futf probe was run in this review. This note is not an inventory, stamp, or cargo-vet record.

## Identity and integration

- WIP native tree `third_party/mbedtls`: Git tree `d0f98ae18f09e22596410a948cfc6b86a6ad8db3`; seam tree `third_party/mbedtls-sys`: `8c5002200784a935e962977c4c723402fbdc033c`. `build_info.h` explicitly reports 3.6.7 / `0x03060700`.
- The local `mbedtls-3.6.7.tar.bz2` SHA-256 is `a7e8bcbec0e6f761b4af24f25677626b35f762f68eef79c08677a363212d11f6`. This review read the tar in memory: 2,039 regular files; its selected `LICENSE`, `include/` and `library/` have exactly 277 files. Their bytes match all 277 files in the WIP working tree, each calculated Git blob matches HEAD, and all Git modes are 100644. This checks local release-archive identity, not a fresh signature check or a security audit.
- WIP parent `c8c1f080ff767a6a93c5147c193ed4f894caf8d0`, integrated Harness `89032106bd35a74a95f1acb19e110f9d55cd5bc7`, and the local `origin/main` ref `e0a8ae09607d361fc0b94d92fe51756238347302` share the same baseline native tree `ac0bdc3283eba59348cc1239b8b9c71f7b134b6b`, seam tree `3f571c68e6f6831e9a45934741b3ccd7a24731c6`, and stream blob `37abcdab5710d50b69842fc471a3535c0c659e56`. Thus there is no difference in these paths between those three commits; this is not a fresh remote fetch or a merge test. It does not remove the known CA/handshake blocker.
- WIP adds 11 native files, modifies 127 native files, adds the config wrapper, modifies two seam files, and changes the libgit2 stream. Integrated `supply-chain/vendored-sources.json` SHA-256 `0e32cab55c5c9c61b2b4ab6502700b2ab01fd07e94cfdc6640fea09603def5d4` is the old identity. Relative to the committed WIP, the `libgit2-sys` capsule encompasses 130 modified and 12 added paths, while `libssh2-sys` and `mbedtls-sys` each encompass 129 modified and 12 added paths. The new config wrapper is one of those 12; the WIP stream affects only `libgit2-sys`. Later worktree edits are outside these counts.
- Integrated Harness `Cargo.lock` SHA-256 `ea15a522ac4d5f66796c1789939a3b4b821c1ac0e5c3a9e246f29f1edabda6c3` and the integrated App lock both say `mbedtls-sys 3.6.2`; App resolves it at Harness SHA `89032106bd35a74a95f1acb19e110f9d55cd5bc7`. The recorded 3.6.2 `mbedtls-sys` capsule digest is `479c843c7ab26876ecd6fbb1a33b54a7967b107c6cdac1754147a1bedd28069d`. Integrating WIP alone therefore cannot be treated as finished lock/inventory/stamp/audit work. Recompute and independently review them only after source is stable; preserve the resulting exact App/Harness graph boundary.

## Source-level findings

1. **Conditional post-fork PSA RNG concurrency candidate, not dynamically reproduced.** In WIP `third_party/mbedtls/library/psa_crypto_random.c:137-168`, `rng->pid` is read before taking `mbedtls_threading_psa_rngdata_mutex`, and written inside that mutex after reseeding. Two newly started threads in a child with inherited initialized PSA state can both observe the old PID: thread A reseeds, releases `rngdata_mutex`, and enters `mbedtls_ctr_drbg_random`; thread B then acquires `rngdata_mutex`, does not recheck the PID, and reseeds again. The reseed path `psa_crypto_random.c:131-132` -> `psa_crypto_random_impl.h:154-160` -> `ctr_drbg.c:504-507` does not acquire the CTR-DRBG mutex used by `ctr_drbg.c:696-712` for generation. The upstream `ctr_drbg.h:444-447` explicitly says reseed cannot run concurrently with generation on that context. In addition, the unlocked PID read can race with the locked PID write. `threading.h:153-158` says RNG global members require the rngdata mutex, whereas `psa_crypto_random.h:44-52` describes the internal generate operation as thread-safe.

   The default WIP enables PSA, CTR-DRBG and TLS 1.3, disables external PSA RNG, and enables pthread threading on Unix/Apple. `psa_crypto.c:4398-4422,8098-8107` connects `psa_generate_random` to the global context; TLS 1.3 calls `psa_crypto_init` (`ssl_tls13_generic.c:40-47`). These facts do **not** establish that the product generates PSA randomness after fork or that a real device reaches this interleaving. The product backend also has its own separate CTR-DRBG; do not conflate it with this global PSA context. The explicit `psa_random_reseed` / `psa_random_deplete` / `psa_random_set_prediction_resistance` additions (`psa_crypto.c:8021-8084`) hold the rngdata mutex while their CTR-DRBG mutations do not use the mutex held by generation; no product call of those APIs was found in the searched Git consumers or `crates/`. This is a source-level concurrency concern requiring independent upstream/configuration review and reachability assessment; no new runtime probe was run and no exploit or affected user has been established.

2. **Pre-existing build environment invalidation gap; a separate bounded fix has received independent review.** The original WIP `third_party/mbedtls-sys/build.rs` uses `cargo_metadata(false)` when compiling the three archives and emits only `IPHONEOS_DEPLOYMENT_TARGET` as an environment watch. The pinned `cc 1.2.62` uses the same metadata switch for `rerun-if-env-changed` (`cc/src/lib.rs:1257-1261,3883-3889`, `cc/src/command_helpers.rs:56-59`). An unchanged Cargo target could therefore reuse archives after compiler/flag/archiver environment changes. The historical `probe-build/incremental-first.log` confirms the old output emitted only the iPhone deployment watch. This was already present in WIP parent, not introduced by the 3.6.7 import. The scoped fix and tiny-build regression in `0159f5553d4103e4ff2a9755a5f9fcc12f6ac5ef` are separate from the immutable WIP identity; root independently reviewed the source and reran the regression. This is not cross-platform or package acceptance. A clean target is still needed when compiler binaries or other untracked build inputs change in place.

## Reviewed behavior and integration differences

- `psa_crypto_random.c`, `psa_crypto_random.h`, and `psa_crypto_random_impl.h` were read end-to-end. Internal generation propagates DRBG errors and splits output into configured request limits; child reseed includes PID/time and reports entropy failure. The synchronization concern above remains open. Relevant SHA-256 values: `psa_crypto_random.c` `36bafe1266fa7ef1dc247cfb59844e451cc6ec27e33dbfd28d316bc234848a16`, `psa_crypto_random.h` `d85ae6878e90f968987a82133a4ad9a0c5a4084f3722d70614142ea21d91ff94`, `psa_crypto_random_impl.h` `34570ef88e61ea09d3234fef8403101051640f12aa131d33b163e9b818074f82`.
- The new `bignum_core_invasive.h` and `cipher_invasive.h` declarations are gated by `MBEDTLS_TEST_HOOKS`, disabled in the default config. `threading_internal.h` contains the internal interface version; `chacha20_internal.h` declares the counter check. In the reviewed regions `chacha20.c:187-305` prechecks the available 32-bit block space and preserves remaining bytes of a final partial block; `chachapoly.c:158-210` performs the same check before changing MAC state. This is a static reading, not an exhaustive cipher audit or vector test.
- `pem.c:243-265` now rejects zero input before reading the last padding byte. `x509_crt.c:488-548` defaults `ca_istrue` to false, bounds the sequence and no longer interprets a leading INTEGER as the CA Boolean. These reviewed paths do not establish overall PEM/X.509 correctness or actual CVE exploitability.
- Exact policy/config differences worth reviewing: the 3.6.7 default X.509 profile adds SHA3-256/384/512 allow-bits (`x509_crt.c:89-97`) to the prior SHA-2 bits. `ssl_tls.c:6123-6128` assigns that profile to the default TLS preset used by the libgit2 backend; actual acceptance of any SHA-3 certificate also depends on the signature OID/parser and verifier paths, which were not established here. The upstream config also newly enables `MBEDTLS_SSL_KEYING_MATERIAL_EXPORT` (`mbedtls_config.h:1781-1792`); this establishes compiled capability, not a product call or secret disclosure. TLS 1.2/1.3 remain enabled; the LingXi wrapper disables self-test and enables pthread locking on supported Unix/Apple, while it leaves PSA-as-default (`MBEDTLS_USE_PSA_CRYPTO`) disabled.
- Original WIP seam `build.rs` / `lingxi_config.h` / `Cargo.toml` were read in full. The wrapper is installed into a generated include tree shared with the consumers; native source enumeration rejects non-regular files and the three static archives are partitioned from sorted `.c` inputs. This reading is not a cross-platform or final-link proof. `lib.rs` was read but unchanged. Original WIP seam SHA-256: build `9e2e4a0a6694f9da9584954a0d016a33a8e03e581705a008c831d8f7a65c846b`, wrapper `0ba399e13235b7b9dbaccf05d1a51b69efa7c7eff1244c6488b7a054c86342cc`, manifest `2ac22094e531fe1d4758d03aa4bdff4f6bbe3b11672a10153dc95610d35a571c`. The new environment-tracking worktree build script necessarily has a different identity.

## Incremental build fix verification

The build script in commit `0159f5553d4103e4ff2a9755a5f9fcc12f6ac5ef` has SHA-256 `c0d04c4e90ad7d01d33d28ae7c71b71295cfc26dee5c4cb3164d3d26cd4cf469`; the new `scripts/tests/test_mbedtls_build_env.py` SHA-256 is `f5f0464d33d248ff53291c78a2542be6a0825aabf93d55867d2e0c03f6fca72a`. The script emits Cargo watches for cc's ordinary, target-triple, underscored-triple, and active HOST_/TARGET_ compiler and archiver inputs, plus its selected tool/flag controls and the listed Apple/MSVC/inherited toolchain environment. It retains `cargo_metadata(false)` and the existing `lib.rs` link declarations. This is a bounded set, not a guarantee that every compiler or SDK environment input or an executable replaced at the same path is detected.

Using Rust/Cargo 1.94.0 and locked cc 1.2.62, the Python regression created a temporary three-C-file fixture using the real revised build script: an unchanged environment remained Cargo Fresh; CFLAGS changes from 1 to 2, an underscored target override to 3, and removal back to 2 changed the linked executable as expected. Target-specific raw/underscored `CC` and underscored `AR` wrappers executed after their environment changes; the captured build metadata contained no `rustc-link-lib` directive. The revised test passed in 9.300 seconds; it additionally asserts the selected RUSTC, archive-date, and WASI environment watches. A separate negative control used the original `3c37ca84` build script with the same fixture: after CFLAGS changed from 1 to 2 the linked executable still returned 1, and the test rejected it. `rustfmt --edition 2021 --check` and `git diff --check` passed. This tests a host fixture and is not a full native or target-platform build.

## Remaining review boundary

No complete source audit was performed. The following 131 exact WIP-changed files have unreviewed content or changes; this count includes files where only selected regions were inspected. For `chacha20.c`, `chachapoly.c`, `ctr_drbg.c`, `psa_crypto.c`, `pem.c`, `x509_crt.c`, `ssl_tls.c`, `mbedtls_config.h`, and `threading.h`, only the particular regions identified above were inspected; all other changes remain open. In particular, the large `ecp_curves.c`/`ecp_curves_new.c` optimized modular arithmetic transition, bignum/RSA changes, TLS state-machine changes, remaining X.509/ASN.1 paths, and configuration-dependent branches need substantive independent review and relevant tests. The backend stream file is outside this list; the bounded review of commit `fb6cbef9790701717e05a5d2e81decb8f59e27f9` is recorded separately, and its later edits need their own review. The known CA race remains a separate blocker. The reviewed `build.rs` change still needs final-graph, cross-platform, and package verification.

```text
third_party/mbedtls/include/.gitignore
third_party/mbedtls/include/CMakeLists.txt
third_party/mbedtls/include/mbedtls/asn1write.h
third_party/mbedtls/include/mbedtls/bignum.h
third_party/mbedtls/include/mbedtls/build_info.h
third_party/mbedtls/include/mbedtls/chacha20.h
third_party/mbedtls/include/mbedtls/chachapoly.h
third_party/mbedtls/include/mbedtls/check_config.h
third_party/mbedtls/include/mbedtls/cipher.h
third_party/mbedtls/include/mbedtls/config_adjust_legacy_crypto.h
third_party/mbedtls/include/mbedtls/config_adjust_legacy_from_psa.h
third_party/mbedtls/include/mbedtls/config_adjust_psa_superset_legacy.h
third_party/mbedtls/include/mbedtls/ctr_drbg.h
third_party/mbedtls/include/mbedtls/debug.h
third_party/mbedtls/include/mbedtls/dhm.h
third_party/mbedtls/include/mbedtls/ecdh.h
third_party/mbedtls/include/mbedtls/ecdsa.h
third_party/mbedtls/include/mbedtls/ecjpake.h
third_party/mbedtls/include/mbedtls/ecp.h
third_party/mbedtls/include/mbedtls/entropy.h
third_party/mbedtls/include/mbedtls/error.h
third_party/mbedtls/include/mbedtls/gcm.h
third_party/mbedtls/include/mbedtls/lms.h
third_party/mbedtls/include/mbedtls/mbedtls_config.h
third_party/mbedtls/include/mbedtls/net_sockets.h
third_party/mbedtls/include/mbedtls/pk.h
third_party/mbedtls/include/mbedtls/pkcs7.h
third_party/mbedtls/include/mbedtls/platform.h
third_party/mbedtls/include/mbedtls/platform_util.h
third_party/mbedtls/include/mbedtls/psa_util.h
third_party/mbedtls/include/mbedtls/rsa.h
third_party/mbedtls/include/mbedtls/ssl.h
third_party/mbedtls/include/mbedtls/ssl_cookie.h
third_party/mbedtls/include/mbedtls/ssl_ticket.h
third_party/mbedtls/include/mbedtls/threading.h
third_party/mbedtls/include/mbedtls/x509.h
third_party/mbedtls/include/mbedtls/x509_crt.h
third_party/mbedtls/include/mbedtls/x509_csr.h
third_party/mbedtls/include/psa/crypto.h
third_party/mbedtls/include/psa/crypto_config.h
third_party/mbedtls/include/psa/crypto_extra.h
third_party/mbedtls/include/psa/crypto_sizes.h
third_party/mbedtls/include/psa/crypto_values.h
third_party/mbedtls/library/.gitignore
third_party/mbedtls/library/CMakeLists.txt
third_party/mbedtls/library/Makefile
third_party/mbedtls/library/aesce.c
third_party/mbedtls/library/aesni.c
third_party/mbedtls/library/alignment.h
third_party/mbedtls/library/asn1write.c
third_party/mbedtls/library/base64.c
third_party/mbedtls/library/bignum.c
third_party/mbedtls/library/bignum_core.c
third_party/mbedtls/library/bignum_core.h
third_party/mbedtls/library/bignum_internal.h
third_party/mbedtls/library/ccm.c
third_party/mbedtls/library/chacha20.c
third_party/mbedtls/library/chachapoly.c
third_party/mbedtls/library/check_crypto_config.h
third_party/mbedtls/library/cipher.c
third_party/mbedtls/library/cipher_wrap.c
third_party/mbedtls/library/cipher_wrap.h
third_party/mbedtls/library/common.h
third_party/mbedtls/library/constant_time.c
third_party/mbedtls/library/constant_time_impl.h
third_party/mbedtls/library/ctr_drbg.c
third_party/mbedtls/library/debug.c
third_party/mbedtls/library/debug_internal.h
third_party/mbedtls/library/dhm.c
third_party/mbedtls/library/ecdh.c
third_party/mbedtls/library/ecdsa.c
third_party/mbedtls/library/ecp.c
third_party/mbedtls/library/ecp_curves.c
third_party/mbedtls/library/ecp_curves_new.c
third_party/mbedtls/library/ecp_invasive.h
third_party/mbedtls/library/entropy_poll.c
third_party/mbedtls/library/error.c
third_party/mbedtls/library/hmac_drbg.c
third_party/mbedtls/library/lmots.c
third_party/mbedtls/library/lms.c
third_party/mbedtls/library/memory_buffer_alloc.c
third_party/mbedtls/library/net_sockets.c
third_party/mbedtls/library/pem.c
third_party/mbedtls/library/pk.c
third_party/mbedtls/library/pk_ecc.c
third_party/mbedtls/library/pk_internal.h
third_party/mbedtls/library/pk_wrap.c
third_party/mbedtls/library/pkcs7.c
third_party/mbedtls/library/pkwrite.c
third_party/mbedtls/library/platform_util.c
third_party/mbedtls/library/psa_crypto.c
third_party/mbedtls/library/psa_crypto_aead.c
third_party/mbedtls/library/psa_crypto_cipher.c
third_party/mbedtls/library/psa_crypto_core.h
third_party/mbedtls/library/psa_crypto_driver_wrappers.h
third_party/mbedtls/library/psa_crypto_ecp.c
third_party/mbedtls/library/psa_crypto_ffdh.c
third_party/mbedtls/library/psa_crypto_mac.c
third_party/mbedtls/library/psa_crypto_rsa.c
third_party/mbedtls/library/psa_crypto_slot_management.c
third_party/mbedtls/library/psa_crypto_storage.h
third_party/mbedtls/library/psa_util.c
third_party/mbedtls/library/rsa.c
third_party/mbedtls/library/rsa_alt_helpers.c
third_party/mbedtls/library/rsa_alt_helpers.h
third_party/mbedtls/library/rsa_internal.h
third_party/mbedtls/library/sha256.c
third_party/mbedtls/library/sha512.c
third_party/mbedtls/library/ssl_client.c
third_party/mbedtls/library/ssl_debug_helpers.h
third_party/mbedtls/library/ssl_debug_helpers_generated.c
third_party/mbedtls/library/ssl_misc.h
third_party/mbedtls/library/ssl_msg.c
third_party/mbedtls/library/ssl_tls.c
third_party/mbedtls/library/ssl_tls12_client.c
third_party/mbedtls/library/ssl_tls12_server.c
third_party/mbedtls/library/ssl_tls13_client.c
third_party/mbedtls/library/ssl_tls13_generic.c
third_party/mbedtls/library/ssl_tls13_keys.c
third_party/mbedtls/library/ssl_tls13_keys.h
third_party/mbedtls/library/ssl_tls13_server.c
third_party/mbedtls/library/threading.c
third_party/mbedtls/library/timing.c
third_party/mbedtls/library/version_features.c
third_party/mbedtls/library/x509_create.c
third_party/mbedtls/library/x509_crl.c
third_party/mbedtls/library/x509_crt.c
third_party/mbedtls/library/x509_csr.c
third_party/mbedtls/library/x509_internal.h
third_party/mbedtls/library/x509write_crt.c
third_party/mbedtls/library/x509write_csr.c
```
