# Recovering compressed conversation requests without changing history

## Problem and evidence

An HTTP request failure can strand a valid conversation when each subsequent turn
replays the same history using the same rejected transport representation. Adding
a short message, restarting, or forking need not change that representation enough
to recover. Deleting valid conversation items is not an appropriate encoding fix.

This investigation used source revision `e20ccdfa80` and public reports available
on 2026-11-17. These reports do not establish a single universal image bug:

| Report | Evidence | What remains unknown |
| --- | --- | --- |
| [#60739](https://github.com/openai/codex/issues/60739) | An image attachment succeeded; rereading it preceded persistent compressed HTTP 400 responses. | No controlled encoding A/B in the original report. |
| [#60594](https://github.com/openai/codex/issues/60594) and its comments | Multiple same-body comparisons report valid zstd failing while identity JSON succeeds, including text-only inputs; existing chats recovered after disabling compression. | The precise backend receiver defect; reports include successful compressed counterexamples. |
| [#61489](https://github.com/openai/codex/issues/61489) | Synthetic high-expansion payloads reportedly fail compressed and succeed uncompressed. | Whole-body ratio is insufficient to explain all failures; a per-block or receiver-buffer mechanism is a hypothesis. |
| [#58497](https://github.com/openai/codex/issues/58497) | Oversized WebSocket replay and an experimental staged-replay approach. | General equivalence of staged replay and production recovery. This is a separate transport problem. |
| [#5346](https://github.com/openai/codex/issues/5346) | An older client retained an invalid image MIME representation after viewing source text as an image. | This does not establish the cause of recent opaque 400s. Current image validation differs substantially. |

Additional symptom reports include [#61097](https://github.com/openai/codex/issues/61097)
(after compaction), [#58569](https://github.com/openai/codex/issues/58569), and
[#58639](https://github.com/openai/codex/issues/58639) (goal continuation). A common
root cause has not been verified. Searches of compression-related commits and PRs
also found separate fixes for compressed rollout lookup and app-server WebSocket
codec negotiation; neither is the HTTP Responses request path changed here.

OpenAI's [public API compression documentation](https://developers.openai.com/api/docs/guides/production-best-practices)
describes decompression limits. Those limits do not by themselves establish the
implementation or failure reason of the ChatGPT Codex endpoint.

## Relevant execution paths

- `core/src/session/mod.rs`: `record_prepared_conversation_items` records history
  and persists response items. Completed tool results are recorded before the next
  sampling request; rejecting that request does not undo the tool operation.
- `core/src/session/rollout_reconstruction.rs`: resume reconstructs retained
  history and checkpoint metadata. A rejected wire encoding is not a corrupt
  rollout, and replaying that rollout can produce the same failing request.
- `core/src/context_manager/history.rs`: model-facing projections and normalization
  prepare the active context, including tool-call/output pairing and attachment
  resolution. Retained history and model projections are already distinct.
- `attachment-store/src/lib.rs`: attachments can remain inline or use resolvable
  file references. Replacing inline attachments with a new storage architecture
  would not address text-only compression failures.
- `protocol/src/models.rs`, `utils/image/src/lib.rs`, and
  `core/src/tools/handlers/view_image.rs`: image decoding, MIME selection, resizing,
  and validation happen before the Responses request is encoded. Invalid local
  images have existing regression coverage. A successfully decoded PNG does not
  prove the receiver can process a compressed JSON body containing it.
- `core/src/client.rs`: HTTP compression is selected for the OpenAI Codex backend
  when enabled. HTTP sends the active prompt; WebSockets can use a previous response
  and incremental input. Reconnection may require full replay, followed by HTTP
  fallback when WebSockets cannot continue.
- `codex-api/src/endpoint/responses_websocket.rs`: current code already classifies
  a size-close before receiving text as `WebsocketMessageTooLarge`. This is newer
  than the implementation described in #58497. This patch does not change it.
- `http-client/src/request.rs`: JSON is serialized into reference-counted bytes,
  compressed with zstd level 3, and prepared before authentication/signing.
- `http-client/src/transport.rs`: unsuccessful HTTP status responses become
  `TransportError::Http` before a successful stream is returned.
- `codex-api/src/endpoint/session.rs`: prepared requests are authenticated and sent
  through bounded transport retry and per-attempt telemetry. Ordinary retry does
  not retry HTTP 400 and does not change its encoding.
- `codex-api/src/endpoint/responses.rs`: the recovery boundary introduced here.
  Both typed requests and JSON requests use it. It has no access to tool execution
  or mutable conversation history.

The CLI uses core directly; IDE clients and the desktop app use core through the
app-server. This shared HTTP boundary protects those callers when they use this
source version. The proprietary UI and backend receiver are outside this checkout.
App-server JSON-RPC WebSocket compression and rollout-file compression are separate
codecs and are unaffected.

## Recovery contract

If a zstd Responses request receives either the exact opaque JSON error
`{"detail":"Bad Request"}` with HTTP 400, HTTP 413, or HTTP 415 before a stream
opens, retry the same serialized JSON once using identity encoding.

The fallback is a diagnostic transport alternative, not a claim that a generic
400 proves a compression defect. It also handles receivers that do not support
zstd. A detailed 400, authentication/policy error, timeout, disconnect, build
failure, or error inside an accepted stream does not qualify.

Invariants:

1. Decompressing the initial request yields exactly the fallback body bytes.
   Images, messages, tool results, checkpoint representations, ordering, and IDs
   are preserved. No history repair, truncation, compaction, or rollback is added.
2. The encoding fallback is attempted at most once per logical request. Each send
   retains the existing bounded transport retry policy. It does not create a new
   sampling loop or increase the existing stream retry budget.
3. Recovery happens before a response stream is exposed. It cannot dispatch a tool
   call, consume an assistant response twice, or rerun completed local operations.
   It relies on the HTTP semantics of an explicit request rejection; it is not a
   general exactly-once guarantee for arbitrary server or proxy implementations.
4. Authentication is reapplied after preparing the identity body. A signature over
   compressed bytes is never reused for uncompressed bytes. Routing/session
   headers remain the same, subject to the existing auth provider's authority.
5. A failed identity attempt returns its error normally, retaining provider status,
   body, and correlation headers. A payload-free warning records the fallback;
   normal request telemetry records each send. No new prompt or image logging is
   introduced.

No persistent feature setting or conversation schema changes. Subsequent requests
may use compression and independently fall back; a resumed thread needs no saved
recovery flag. This favors compatibility over a global or permanent compression
downgrade. The cost is one rejected compressed upload when the problem recurs and
temporarily retaining both raw and compressed bytes during the first attempt.

## Reproduction and validation

The new `codex-api/tests/responses_compression.rs` fixture injects a receiver that
rejects compressed requests and accepts identity JSON. It reproduces the client
failure deterministically without credentials or private transcripts. The initial
regression failed on the original implementation with the reported 400 and passes
with recovery. A real local HTTP server exercises status-to-error conversion too.

The tests verify exact decoded bytes, repeated image content, retained tool outputs,
opaque compaction content, repetitive payloads around 128 KiB and up to 2 MB,
persistent rejection, detailed errors, status classification, signing, routing,
build failure, disconnects, and failure after a stream opens. The synthetic sizes
are test inputs, not assertions about backend limits. The compaction fixture tests
transport preservation of a checkpoint, not the compaction algorithm itself.

The core integration regression in `core/tests/suite/request_compression.rs`
drives a real image tool, rejects its continuation, persists and restarts the same
thread, then recovers with identity encoding. It checks thread identity, retained
attachment and tool-result images, the original user decision, exact fallback
bytes, and a single image-tool execution across the restart.

Commands from `codex-rs`:

```sh
cargo test -p codex-api
RUST_MIN_STACK=33554432 cargo test -p codex-core --test all suite::request_compression
cargo test -p codex-utils-image
cargo test -p codex-http-client request::tests
```

Validation results and environment details are recorded in the contribution
proposal alongside this report.

## Limits and maintainer follow-up

This is verified client recovery under controlled rejection, not a local
reproduction or repair of the proprietary decompressor. No authenticated production
probe or private conversation replay was performed. Maintainers should correlate
the public reports' request IDs to establish the receiving component and rejection
reason, then rerun their synthetic A/B cases against this change.

Identity encoding cannot repair invalid image bytes, missing attachment files,
corrupt JSONL, unusable checkpoints, expired references, an oversized plain body,
or a backend outage. Such failures remain errors; this patch never guesses which
history items to discard. Existing image validation, attachment diagnostics,
normalization, and checkpoint reconstruction remain responsible for their own
invariants. Generic quarantine based only on HTTP 400 would risk silent context
loss and is deliberately excluded.

For an existing installation affected specifically by compression, disabling
`features.enable_request_compression` and restarting can be a nondestructive
workaround in the same thread, as reported in #60594. Preserve the original rollout
and completed tool results; changing an image file on disk does not replace bytes
already retained in history. A persistently rejected identity request still needs
its actual validation or backend cause diagnosed.

Further work should improve backend error specificity and test oversized
WebSocket replay independently. Staged replay needs proof of equivalence across
tool-call groups, instructions, checkpoint content, and authentication boundaries
before it can safely become a general recovery mechanism.
