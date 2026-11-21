# Architecture

zchat separates a portable messaging contract from backend clients and the user interface.
An account has a local identifier, a provider identifier and a plugin identifier. A request
names that account, one operation and an open parameter object. Provider-native identifiers
remain opaque strings; the host never merges unrelated identities by display name.

## Execution

The desktop, CLI and MCP call the same `Engine`. It validates the request, assigns its
actual origin, checks the account's AI policy and routes to a provider. The built-in bundle
uses the same provider implementation that it exports through zplugin. A separately loaded
plugin owns its state and returns JSON through a versioned C ABI.

The GUI owns the native window on its creation thread. A worker owns the engine and all
non-Send plugin instances. Bounded channels connect the two; network requests never run in
the window event loop. The desktop redraws after input or changed state and waits when idle.

`zchat-core` provides strict JSON, the operation catalogue, request policies and a bounded
event journal. Both the token tape and owned output have explicit limits. Duplicate decoded
JSON keys are rejected. Numbers preserve their exact textual representation so remote IDs
do not pass through floating point. Journal cursors are monotonic, and expired cursors return
an error rather than silently losing a gap.

## Capabilities and extensions

The common contract covers accounts, conversations, members, messages, attachments,
presence, typing, search, calls and ordered events. A plugin manifest reports a supported
subset. Unsupported operations return `unsupported_operation`; their presence in the common
catalogue does not assert implementation by a particular provider.

`native.invoke` carries the complete documented backend request. Telegram uses TDLib JSON,
Matrix uses the Client-Server API, SimpleX uses its official CLI command protocol, and Delta
Chat uses positional JSON-RPC. Native responses preserve their original shape. This leaves
room for new platform-specific functions without changing the common ABI. New visual/media
capabilities still require concrete renderer and media integration in the client.

## Ownership and trust

`ImPlugin` has two methods: `manifest` and `invoke`. Inputs borrow bytes for a single call;
outputs use `OwnedBytes`, and the host receives a library-leased buffer. The host creates
owned plugin instances with `Rc<zplugin::Plugin>`. Native libraries stay pinned because a
backend may own threads, callbacks or transport resources.

Native libraries are loaded only after explicit local trust registration. This is trusted
native code execution, not a sandbox. A future sandboxed transport must define its own
network, filesystem and permission boundary rather than inherit native privileges by
accident. The present host does not claim that an ABI metadata check makes a library safe.

MCP fixes the request origin to AI. Read access is opt-in per account; writes and native
extensions require both account and server grants. Local CLI/desktop control uses the human
entry point and is intended for the account owner. These local entry points do not provide
isolation from another process already running as that same operating-system user.

## State and reliability

The host stores account and plugin configuration, with environment references for secrets.
It does not persist message bodies. Platform engines may maintain encrypted databases,
keys and sync state in their own directories. Configuration saves are atomic and use private
Unix permissions.

Requests, event queues and subprocess responses are bounded. Transports have deadlines and
retain correlation identifiers. A successful response is distinct from final server delivery;
Telegram sends, for example, initially report queued state and later emit delivery events.
The host caches a bounded set of successful write results within one process. Matrix also
uses the caller's stable idempotency key as its server transaction ID. No automatic blind
retry is performed after an ambiguous mutation timeout.

Encryption stays with the actual platform engine. The direct Matrix HTTP adapter checks
room encryption before sending plaintext and rejects encrypted rooms. SimpleX and Delta
Chat use their official cores. Threema Gateway defaults to E2E transport requiring an
already encrypted payload; Basic mode is an explicit, separate configuration.

## Dependency and release identity

Development dependencies use each Project Z repository's canonical Git URL without a
registry version selector. `Cargo.lock` records exact validated commits. Publication runs
from an immutable source commit and creates a separate manifest projection after matching
the locked upstream source to its published crate. Source, crate and native-package receipts
must identify the same zchat commit. Release archives contain native artifacts; the Git
repository contains source and text evidence.
