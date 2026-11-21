# zchat-plugin-api

This crate freezes the `projectz.zchat.im` version 1 interface in zplugin's
target-platform C ABI. Both methods exchange bounded UTF-8 JSON:

| Method | Input | Output |
| --- | --- | --- |
| `manifest` | None | Actual operations, backend requirements and plugin metadata |
| `invoke` | One request | One result or structured application error |

The procedural macro exports `ImPluginApi`, `ImPluginBinding`, `ImPluginProxy`
and `ImPluginExport<T>`. Implementations return
`zplugin::OwnedBytes::from_vec(json.into_bytes())`. The generated host proxy
returns `zplugin::Buffer<'a>`, which dereferences to bytes and retains a library
lease until the original allocator releases the output.

The host loads a trusted `.so`, `.dylib` or `.dll` through `zplugin::Plugin`, then
creates the typed proxy with `ImPluginApi::create(&library)`. A persistent owning
proxy can use `Rc<Plugin>` with `Plugin::create_owned` and `ImPluginApi::wrap`.
Calls stay on the proxy's creating thread. Library lifetime and ABI checks do
not sandbox native code; only explicitly trusted native plugins may be loaded.

`MAX_REQUEST_BYTES` and `MAX_RESPONSE_BYTES` are 1 MiB. Hosts check the request
before calling a plugin and check the response before JSON decoding. Platform
limits can be lower. Native methods retain platform-specific schemas so future
capabilities can be added without changing this two-method ABI.
