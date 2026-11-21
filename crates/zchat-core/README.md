# zchat-core

The shared messaging contract for zchat uses `no_std + alloc` and Project Z's
`zconv` JSON validator. It retains platform-specific fields and exact numeric
identifiers, while bounding input bytes, JSON depth, token count and object size.

`Request` names an account, an operation and an open parameter object. The common
operation catalog covers messages, conversations, media, presence and calls.
A plugin advertises the subset it actually implements. Documented platform
extensions use `native.invoke`; a catalog entry alone is not a working adapter.

```rust
use zchat_core::{Request, RequestPolicy};

let request = Request::parse(r#"{
  "account":"personal",
  "operation":"conversation.list",
  "params":{"limit":50},
  "actor":"human"
}"#)?;
RequestPolicy::read_only().authorize(&request)?;
# Ok::<(), zchat_core::ChatError>(())
```

The host supplies the policy from authenticated context. AI access, writes,
destructive actions and native methods each have a separate gate. The request's
actor label does not authenticate the caller. MCP hosts must set the AI actor
themselves. An optional idempotency-key requirement validates the key's presence;
the provider remains responsible for implementing server or local deduplication.

`EventJournal` keeps ordered updates within an event-count and encoded-byte
budget. Polling returns a resumable cursor, and a cursor behind an eviction
returns `cursor_expired`. Repeated account/kind/event identifiers are deduplicated
within the retained window; reusing an identifier with changed payload returns
`event_conflict`. Journal cursors are scoped to one instance. Persisted provider
sync tokens and a host stream identity handle reconnection across restarts.

Run the core tests with `cargo test -p zchat-core`. The `core` benchmark emits
JSON timing records for parsing, policy checks, bounded appends and page reads.
Its results describe those workloads on the machine that ran them.
