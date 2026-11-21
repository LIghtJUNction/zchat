# zchat-providers

`zchat-providers` connects the zchat interface to existing messaging clients and services. It builds as both a Rust library and a `zplugin` dynamic plugin. Accounts keep their own connection state; the host supplies configuration and decides which accounts and operations an AI caller may access.

The common interface covers the operations implemented by each adapter. `native.invoke` exposes the underlying protocol API for features that have no common mapping. A native method that negotiates a call or transfers an encrypted payload does not supply a call engine or cryptographic implementation by itself. Inspect `ProviderHub::catalog()` and an account's `capabilities` before choosing an operation.

## Implemented adapters

| Provider ID | Actual backend | Common operations and limits |
| --- | --- | --- |
| `telegram` | Official TDLib C JSON interface, loaded with `zsys::dylib` | Authentication state, explicit login, chat/history listing, text, edits, deletion, reactions, read state, typing and updates. TDLib manages its protocol and encryption. The host must show channel sponsored messages and report their views. AI access to Telegram content is disabled under the current API terms. |
| `matrix` | Matrix Client-Server HTTPS API through system `curl` | Joined rooms, timeline history, text, edits, redactions, reactions, receipts, typing and timeline sync. The adapter checks room encryption before text sends and refuses encrypted rooms. It does not implement a Matrix crypto device, key verification, media upload or calls. |
| `xmpp` | libstrophe 0.12+ public C ABI, loaded with `zsys::dylib` | Authenticated TLS connection, roster, discovered server features, text, requested displayed markers, typing, group presence and incoming stanzas. History is an explicitly marked, bounded session cache. OMEMO, archive normalization and media are not implemented. |
| `deltachat` | Official `deltachat-rpc-server`, persistent stdio JSON-RPC | Chats, history, text, edits, deletion, reactions, read state, joining/leaving and events. The external chatmail core owns account storage, encryption and network transport. Its native methods use positional parameter arrays. |
| `simplex` | Official SimpleX CLI localhost WebSocket API | Active user, latest chat previews, message history, text, edits, deletion, reactions, read state and events. The CLI owns encryption and account storage. Conversation preview pagination beyond the bounded latest window is not normalized. |
| `threema-gateway` | Official Threema Gateway HTTPS API | A separate Gateway identity, credit inspection and outbound basic text. E2E transport accepts ciphertext produced by a separate crypto implementation. This adapter does not log into an ordinary Threema account or provide personal history, incoming callbacks or calls. |
| `session` | No bundled transport | Catalog entry is `unavailable`. Configuration returns `backend_unavailable`. A separate libsession integration is required. |

The source contract is documented by [TDLib](https://core.telegram.org/tdlib/docs/td__json__client_8h.html), [Matrix](https://spec.matrix.org/latest/client-server-api/), [libstrophe](https://strophe.im/libstrophe/doc/0.12.0/strophe_8h.html), [chatmail JSON-RPC](https://js.jsonrpc.delta.chat/classes/RawClient.html), [SimpleX commands](https://github.com/simplex-chat/simplex-chat/blob/stable/bots/api/COMMANDS.md), [Threema Gateway](https://gateway.threema.ch/en/developer/api), and [libsession](https://github.com/session-foundation/libsession).

## Account configuration

Configuration is lazy: `account.configure` validates and stores settings without connecting, and returns `connected: false`. Use `account.status` to attempt the configured connection. The host must persist configuration securely and restore it when starting a new provider instance. The catalog and account list contain no stored credentials.

```json
{
  "account": "work-matrix",
  "operation": "account.configure",
  "actor": "human",
  "params": {
    "provider": "matrix",
    "config": {
      "homeserver": "https://matrix.example.org",
      "access_token": "supply-through-the-host-secret-store"
    }
  }
}
```

| Provider | Required configuration | Optional configuration |
| --- | --- | --- |
| Telegram | `api_id` as a positive number; `api_hash`; `database_directory` | `library_path`; `files_directory`; `database_encryption_key`; `phone_number` |
| Matrix | `homeserver`; `access_token` for authenticated operations | Initial sync `cursor` |
| XMPP | `jid`; `password` | `library_path`; alternate `host`; `port` (`0` delegates discovery to libstrophe); `cafile` |
| Delta Chat | A usable official backend executable and local account | `command` defaults to `deltachat-rpc-server`; literal `args` array; `accounts_path`; numeric `account_id` defaults to `1` |
| SimpleX | A running official CLI with its localhost API enabled | `url` defaults to `ws://127.0.0.1:5225`; numeric `user_id` defaults to `0`, which verifies and adopts the CLI's active user |
| Threema Gateway | `identity`, an eight-character Gateway ID beginning with `*`; `secret` | `mode` is `basic` or `e2e`, default `e2e`; `base_url` defaults to the official Gateway |

Telegram needs an independently registered application ID. Saving `phone_number` does not request a code. Explicit `account.login` advances the current challenge using `params.phone_number`, `params.code`, or `params.password`; other challenges remain available through TDLib's native API. `account.status` reports authorization state without initiating phone verification.

Delta Chat receives `accounts_path` through `DC_ACCOUNTS_PATH`, not a shell command. The adapter starts network IO before common messaging operations. Use native account-management methods to create or configure an account when needed. Keep a SimpleX connection on a dedicated CLI profile: common operations verify the selected user, while native commands can access the full CLI state.

## Calling the interface

```rust,no_run
use zchat_core::{ChatError, parse};
use zchat_providers::ProviderHub;

fn main() -> Result<(), ChatError> {
    let mut hub = ProviderHub::new();
    let catalog = ProviderHub::catalog();
    println!("{}", catalog.try_stringify()?);

    hub.invoke(&parse(r#"{
      "account":"work",
      "operation":"account.configure",
      "actor":"human",
      "params":{"provider":"matrix","config":{
        "homeserver":"https://matrix.example.org",
        "access_token":"example-only"
      }}
    }"#)?)?;
    Ok(())
}
```

Common request parameters are `conversation_id`, `message_id`, `text`, `limit`, `cursor`, and, for mutations, `idempotency_key`. A message list returns `messages` with string identifiers, `sender`, `text`, `conversation_id`, and its declared order. Pagination cursors are opaque to the host and belong to their provider and operation. Matrix uses `idempotency_key` as the native transaction ID. Other protocols do not all provide equivalent deduplication: the host must keep its mutation journal and treat a timeout as an uncertain outcome.

Common results omit redundant `raw` fields unless `params.include_raw` is true. Native results preserve the native response. Missing common operations fail with `unsupported_operation`; an adapter never returns fabricated success. Queue acceptance is reported as `queued` or `accepted`, and delivery must be established from the backend's events.

### Native request examples

These objects are the `params` value for `operation: "native.invoke"`. Each invokes an actual documented backend operation. The host should grant native access separately because the same interface also permits mutations.

| Provider | Read-only example |
| --- | --- |
| Telegram | `{"request":{"@type":"getAuthorizationState"}}` |
| Matrix | `{"method":"GET","path":"/_matrix/client/v3/account/whoami"}` |
| XMPP | `{"xml":"<iq type='get' to='example.org'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>","wait_iq":true}` |
| Delta Chat | `{"method":"get_all_account_ids","params":[]}` |
| SimpleX | `{"command":"/u"}` |
| Threema Gateway | `{"method":"credits"}` |

Telegram also accepts `{"method":"getChat","params":{"chat_id":123}}`. Matrix accepts an optional JSON `body`; requests stay within `/_matrix/` paths and plaintext send paths retain the encryption check. XMPP parses a single `message`, `presence` or `iq` stanza using libstrophe, adds an ID when absent, and correlates requested IQ replies with both ID and sender. Set `wait_iq: false` for asynchronous handling through `events.poll`.

Threema native methods are `credits`, `pubkeys`, `capabilities`, `send_simple` and `send_e2e`. Method arguments can be nested in `params`. `send_simple` requires explicitly configured `basic` mode. `send_e2e` requires `e2e` mode, a recipient `to`, a 24-byte hexadecimal `nonce`, and a bounded hexadecimal `box`; preparing or decrypting that ciphertext is outside this adapter.

## Runtime boundaries

All protocol calls stay on the provider's owning thread. Native XMPP handles are thread-affine. The TDLib receive loop is serialized across accounts within one plugin image, and updates are routed by client ID. A host must select one provider image for all of its Telegram accounts; separate copies of this bundle must not consume the same TDLib process-wide receive queue.

JSON wire records and final responses are limited to 1 MiB. XMPP XML stanzas are limited to 256 KiB. Event queues hold at most 256 records and 512 KiB. If events are evicted, the result contains `dropped_events` and `resync_required`; callers must resynchronize instead of assuming a complete stream. XMPP's session cache is additionally capped at 256 messages and 512 KiB, and always reports `partial_history: true`.

HTTPS uses certificate verification and system `curl`, with fixed process arguments. Headers, bodies and any protocol-mandated query authentication travel over curl's stdin configuration; they are not inserted into shell commands or process arguments. Redirects are not followed. Plain HTTP is accepted only for loopback fixtures. SimpleX's unauthenticated API is restricted to loopback WebSocket addresses. XMPP requires TLS without a certificate-trust override.

The stdio adapter uses bounded writer and reader channels, request IDs and deadlines. A pending event request does not block another command. Late responses cannot satisfy a newer request. Shutdown stops the owned process and bounds IO-thread cleanup; Unix backends receive a separate process group so inherited child pipes can be closed safely.

The dynamic plugin exports `projectz.zchat.im` through `zplugin` and uses the package version in its ABI descriptor. The image is pinned because backend workers or callbacks may outlive a call. Hosts should release accounts and plugin instances during shutdown. They must also validate the actor and account permissions themselves; an `actor` string is not an authentication mechanism.

## Platform rules and dependencies

Telegram's [API terms](https://core.telegram.org/api/terms) apply to third-party clients, including restrictions on AI use, read/online behavior and sponsored messages. The bundled provider disables content operations for AI actors; login status and explicit authentication remain separate. A human client must preserve read state, typing and sponsored-message obligations. Channel history includes `sponsored_messages`; visible sponsored messages use TDLib `viewMessages`, and clicks use `clickChatSponsoredMessage`.

SimpleX integrations must follow its [third-party client conditions](https://simplex.chat/privacy/), keep the underlying CLI current and identify their origin appropriately. XMPP feature support depends on server and peer discovery; the adapter exposes actual [XEP-0030](https://xmpp.org/extensions/xep-0030.html) results rather than assuming every server supports every extension.

This crate is MIT licensed. External clients and native libraries retain their own licenses and terms; they are not embedded in this crate. Install TDLib, libstrophe, chatmail core, or SimpleX from their official distributions. The zchat release packages contain the adapter, not a claim that every optional backend or protocol feature is available on every target.

## Verification

`cargo test -p zchat-providers` includes loopback HTTP and WebSocket fixtures, a fake stdio JSON-RPC process, Telegram C-ABI function fixtures, and a network-free libstrophe C-ABI fixture. Tests exercise correlation, multi-account routing, TLS flags, callbacks, uncertain timeouts, bounded records, subprocess cleanup, JSON normalization, and authentication side effects. Python is required for stdio fixtures; Unix C-ABI tests compile a temporary shared library with `cc`. Generated libraries are removed from the temporary directory and are never repository artifacts.

These tests verify the adapter and host boundaries without live messaging accounts. They do not establish live-account acceptance by a provider, successful login for an arbitrary deployment, or support for all features of an IM service. The workspace acceptance runner separately checks the CLI, MCP policy and dynamic-plugin paths against a loopback Matrix fixture.
