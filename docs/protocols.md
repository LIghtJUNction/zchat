# Protocol sources and adapter boundaries

These links identify the actual interfaces used by the provider bundle. Backend schemas and
platform policies can change; check the installed backend version before extending a mapping.
Native extension coverage depends on each adapter's supported methods and payload formats.

## Telegram

- [TDLib JSON C interface](https://core.telegram.org/tdlib/docs/td__json__client_8h.html)
- [TDLib initialization](https://core.telegram.org/tdlib/getting-started)
- [Telegram API terms](https://core.telegram.org/api/terms)

zchat uses `td_create_client_id`, `td_send` and the process-wide serialized `td_receive`.
`@extra` correlates responses, while `@client_id` routes updates to the correct account.
Authorization state remains visible to the user. The adapter closes clients before dropping
its account state. A supplied TDLib library stays pinned for the process lifetime.

Each developer needs an independent API identity. Normal read, typing, online and
self-destruct behavior must follow the platform. Channel sponsored content must be displayed
and reported through the matching TDLib API. Current API Terms section 1.5 restricts AI use
of platform data; the zchat AI entry point does not return Telegram content.

## Matrix

- [Client-Server specification](https://spec.matrix.org/latest/client-server-api/)
- [Official Matrix Rust SDK](https://github.com/matrix-org/matrix-rust-sdk)

The HTTP adapter uses authenticated HTTPS, preserves sync cursors and transaction IDs,
and maps common room/message operations. It checks the `m.room.encryption` state before
plaintext sends, including raw send requests. A complete encrypted-room client needs device
verification, key sharing, recovery and crypto state; these are outside the current direct
HTTP adapter and must be supplied by a crypto-capable plugin.

## XMPP

- [libstrophe](https://xmpp.org/software/libstrophe/)
- [Service discovery, XEP-0030](https://xmpp.org/extensions/xep-0030.html)
- [XMPP extension catalogue](https://xmpp.org/extensions/)

XMPP capabilities vary by server, room and peer. TLS, authentication, roster/message state
and individual extension results must be distinguished. Raw stanzas preserve extension
access; a raw stanza channel does not by itself implement OMEMO or a Jingle media engine.
`message.list` returns only the bounded current-session `session_cache`, with
`partial_history: true`; it does not retrieve the server's MAM archive.

## SimpleX

- [Official WebSocket API](https://github.com/simplex-chat/simplex-chat/blob/stable/bots/README.md)
- [Current command grammar](https://github.com/simplex-chat/simplex-chat/blob/stable/bots/api/COMMANDS.md)
- [Third-party and infrastructure terms](https://simplex.chat/privacy/)

The official CLI runs the same core API used by official apps. zchat accepts its unauthenticated
WebSocket only on loopback, matches `corrId`, and separates asynchronous `resp` events from
command replies. Use a compatible official CLI and comply with its protocol/version and
network attribution requirements. zchat does not embed or relicense the official core.
`conversation.list` returns a bounded window of the latest chats and does not support a
pagination `cursor`.

## Delta Chat

- [chatmail core and RPC server](https://github.com/chatmail/core)
- [Generated current RPC methods](https://js.jsonrpc.delta.chat/classes/RawClient.html)

The adapter owns a persistent `deltachat-rpc-server` process. Requests use JSON-RPC positional
parameter arrays. Common calls include `get_all_account_ids`, `get_chatlist_entries`,
`get_message_ids`, `misc_send_text_message` and `get_next_event`. A background response reader
keeps a pending event request from blocking other RPC calls. Official account/crypto state
remains with the core.

## Threema

- [Gateway API](https://gateway.threema.ch/en/developer/api)
- [Official Gateway Rust SDK](https://gateway.threema.ch/en/developer/sdk-rust)
- [Personal-client App Remote Protocol](https://threema-ch.github.io/app-remote-protocol/)

The bundled `threema-gateway` adapter uses a distinct paid API identity beginning with `*`.
Its Basic mode sends plaintext to the Gateway for server-side encryption and must be chosen
explicitly. E2E transport accepts a caller-prepared authenticated ciphertext and nonce.
The adapter does not implement personal-account pairing, key management, personal message
history or an inbound callback server.

## Session

- [Official libsession](https://github.com/session-foundation/libsession)
- [Official desktop architecture](https://github.com/session-foundation/session-desktop/blob/dev/ARCHITECTURE.md)

The official library supplies protocol/crypto/configuration primitives. A complete client
also needs transport, routing, storage, envelopes and group state. zchat currently exposes
an unavailable descriptor for this integration and accepts a separate native plugin when
implemented. No undocumented generic Session RPC service is assumed.
