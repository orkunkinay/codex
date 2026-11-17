# Issue / patch proposal: preserve conversations after compressed request rejection

Suggested title: **Retry rejected compressed Responses requests with identical
uncompressed JSON, preserving the existing conversation**

The current [contribution policy](contributing.md) excludes external code
contributions and PRs. This local patch series is prepared as supporting evidence
for an issue discussion or public fork. Nothing has been pushed, published, or
submitted to the maintainers.

## Proposed issue text

Some existing conversations become unable to continue after HTTP Responses starts
returning `400 {"detail":"Bad Request"}`. Subsequent text-only prompts replay the
same active context with the same encoding. Reports in #60594 show valid compressed
representations failing while identical JSON succeeds uncompressed; #60739 reports
the same symptom after rereading an attachment. This does not establish that all
400s, image failures, or replay failures have one cause.

A deterministic receiver fault fixture reproduces the client failure without
account credentials: reject zstd requests with the reported 400, accept plain JSON,
and submit a history containing a user image plus the completed image-tool result.
The original client returns the rejection. With the patch, the same request
completes without changing a history item or executing a tool again.

The proposed change retains the existing serialized JSON and retries it once
without compression after an opaque 400, 413, or 415 rejection before streaming.
It preserves detailed validation and policy errors, does not reinterpret
disconnects as rejections, and cannot trigger this recovery after a stream opens.
The fallback is prepared and authenticated independently so body signatures stay
valid. Failed identity requests retain their normal error and correlation data.

A compaction regression also recovers a rejected remote compaction request,
persists the resulting checkpoint, resumes the thread, and verifies that the
next recovered request includes both the checkpoint and retained user decision.

The image regression goes further: reject both encodings after a real image tool
runs, shut down the failed conversation, restart that same persisted thread, then
accept the identity request. It verifies the same thread ID, unchanged original
user content and completed tool output (including both image representations),
exact fallback bytes, and exactly one image-tool execution across the restart.

No persistence migration, new production dependency, global compression downgrade,
history deletion, or speculative content quarantine is required. The production
change is confined to `codex-api/src/endpoint/responses.rs`. A zstd dev dependency
is added only to verify lossless request reconstruction in tests.

Maintainer request: correlate the public reports' server request IDs to identify
the receiver defect, evaluate this bounded client protection, and expose specific
decompression/validation errors. The local work does not claim to fix proprietary
server code or to reproduce the production decoder itself.

## Validation record

Base: `e20ccdfa80`. Host: macOS arm64. Rust: repository-pinned `1.99.1`.
All request bodies and artifacts used in regression tests are synthetic.

| Check | Result |
| --- | --- |
| New replay regression against original implementation | Failed with the injected opaque HTTP 400, as expected |
| API crate, including nine encoding recovery tests | 244 passed |
| HTTP client crate | 136 passed |
| Image utility crate | 15 passed |
| Core request compression, including failed-thread restart and compaction recovery | 4 passed |
| Core image-view suite | 20 passed |
| Core WebSocket client and runtime-auth suites | 72 passed |
| Core resume suite | 7 passed |
| Core compaction/resume/fork suite | 2 passed |
| Core remote compaction suite | 25 passed |
| API Clippy (`--lib --test responses_compression -- -D warnings`) | Passed |
| Rustfmt check of changed Rust files; `git diff --check` | Passed |

These targeted suites total **525 passing tests**. The three baseline failures
described below are additional exploratory cases and are not included in that
passing total.

The full repository workspace could not initially resolve because Cargo fetched
unused V8 Chromium submodules from a host returning HTTP 406. Tests therefore ran
in an isolated copy with workspace members narrowed to `codex-api` and `core`,
excluding the unused V8 git patch. The production source and regression test files
were copied verbatim; the repository lockfile supplied dependency versions, and
a package/version/source comparison confirmed no differing resolved dependency
versions. This is targeted crate validation, not a full-workspace or cross-platform
pass.

Builds used `CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0` to fit available disk
space. Core tests used `RUST_MIN_STACK=33554432`; an initial run without an enlarged
test-thread stack aborted with stack overflow. The repository's `just test` also
sets an enlarged stack. The macOS linker reported a large unwind-section warning.

The exact commands, from the isolated `codex-rs` copy, were:

```sh
RUST_MIN_STACK=33554432 CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test -p codex-api -p codex-http-client -p codex-utils-image
RUST_MIN_STACK=33554432 CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test -p codex-core --test all suite::request_compression
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  cargo clippy -p codex-api --lib --test responses_compression -- -D warnings
```

The already-built core integration binary was also run with each of these filters
and `--test-threads=4`: `suite::view_image::`, `client_websockets`,
`suite::resume::`, `compact_resume_fork`, and `suite::compact_remote::`.

An exploratory wider `view_image` filter included three additional code-mode
cases that failed because the `exec` custom tool was unavailable. The focused
image-view suite passed. The same three cases also failed with the original API implementation, with
the same unavailable-`exec` behavior (20 passed, 3 failed in both runs). They are
baseline failures outside this patch; the overall exploratory filter is not
reported as passing.

## Review boundaries

- A successful fallback is not proof that the original error came from zstd.
- The fallback can increase upload bandwidth and briefly retains raw plus
  compressed bytes. It adds at most one encoding change; ordinary transport
  retry limits remain in force.
- Automatic removal of malformed content is excluded because a generic 400
  cannot identify an offending entry reliably.
- Invalid attachments, corrupt persistence, oversized identity requests, and
  ambiguous execution after disconnection remain separate failure classes.
- Windows and Linux were not exercised locally. Proprietary desktop/IDE builds
  and the authenticated production endpoint were not exercised.

See [the investigation](conversation-recovery.md) for architecture paths,
invariants, related reports, reproduction limits, and follow-up work.
