# hearth.v1 protocol

## Transport

Any reliable byte stream works:

- the Unix socket `$XDG_RUNTIME_DIR/hearth/server.sock`;
- TCP, if the listener is enabled;
- stdio of `hearth proxy`, which is how the SSH transport connects.

Each frame is laid out as follows. All integers are little-endian.

```
u32 length   (bytes after this field, 1..=16 MiB)
u8  kind
... body
```

| kind | name | body |
| --- | --- | --- |
| 1 | JSON | UTF-8 JSON object |
| 2 | Data | `u32 sub, u64 offset, bytes`: terminal output starting at stream `offset` |
| 3 | Snapshot | `u32 sub, u64 offset, bytes`: VT state valid at `offset`; Data continues from there |
| 4 | Input | `u32 sub, u64 seq, bytes`: client keystrokes/paste for the terminal behind `sub` |

## Handshake

The client sends a hello first:

```json
{"type":"hello","protocol":"hearth.v1","min_version":1,"max_version":1,
 "client":{"kind":"app|cli","name":"…","version":"…"}}
```

The server answers with one of two messages:

- `{"type":"welcome","version":1,"client_id":"c3","principal":"uid:1000",
  "server":{"version","incarnation","pid","host","user"},"limits":{…}}`
- `{"type":"reject","code":"version|identity|protocol","reason":"…"}`

## Requests

A request looks like this:

```json
{"type":"request","id":7,"method":"session.create","params":{…}}
```

The response looks like this:

```json
{"type":"response","id":7,"ok":true,"result":…}
```

On failure, `ok` is false and `"error":{"code","message"}` replaces the result.

- Error codes: `invalid_params`, `not_found`, `ambiguous`, `unknown_method`,
  `denied`, `timeout`, `failed`, `not_ready`.
- Requests on one connection are handled in order. The exceptions are
  `terminal.wait`, `terminal.wait_text`, `action.run` and `config.reload`,
  which complete asynchronously.
- Methods, parameter schemas and result schemas are described by the running
  server: `api.list` and `api.describe`.
- Unknown parameters are rejected.

## Events

```json
{"type":"event","seq":42,"event":"session.changed","data":{…}}
```

Events that carry `seq` come from the event bus. Their sequence numbers are
contiguous and increasing.

- `session.bootstrap` returns `seq` and full session views, and subscribes
  the connection to events in the same step. Apply events with a higher
  `seq`.
- `session.changed` carries a full session view with `revision`. Ignore
  views older than the one you hold.

Bus events: `session.changed`, `session.closed`, `terminal.title`,
`terminal.cwd`, `terminal.status`, `terminal.prompt`, `terminal.bell`,
`terminal.clipboard`, `terminal.activity` (at most 1/s per block),
`terminal.exit`, `terminal.eof`, `client.connected`, `client.disconnected`,
`config.reloaded`, `server.stopping`.

Per-connection events have no `seq` and travel in order with that
connection's Data frames:

- `terminal.attached` `{sub, mode: snapshot|resume, offset, generation, cols, rows}`
- `terminal.resized` `{sub, cols, rows, owner, offset}`
- `terminal.lost` `{sub}`: the viewer fell behind. Re-attach.
- `input.ack` `{sub, seq, delivered}`
- `client.action` `{action, args}`: sent only to apps with Remote Control
  enabled.

## Terminal streams

1. `terminal.attach {block, cols?, rows?, resume?: {generation, offset}}`.
2. If `resume` names the current generation and the offset is still
   retained (4 MiB ring), the server sends Data from that offset.
3. Otherwise it sends a Snapshot at `S`, followed by Data from `S`. Data
   offsets are contiguous: `offset == previous offset + previous length`.
4. The client feeds the snapshot to a fresh emulator sized `cols × rows`.
   It must strip terminal queries from all stream bytes, because the server
   answers them.
5. Input frames are acknowledged with `input.ack`. After a disconnect, input
   that was never acknowledged is not resent automatically.
6. A slow viewer whose queue exceeds 8 MiB loses that subscription
   (`terminal.lost`). Control-queue overflow closes the connection. The
   program and the other viewers are not slowed.

## Size ownership

- The first viewer to report a size for an unowned block, or the viewer that
  types, owns the block. `block.own` can take, request or release ownership
  explicitly.
- Only the owner's `terminal.resize` changes the PTY size.
- If the owner disconnects, ownership passes to another viewer that has a
  size preference.
