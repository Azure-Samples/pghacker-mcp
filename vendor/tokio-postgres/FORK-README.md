# `tokio-postgres` — vendored fork

This is a **patched copy of the upstream `tokio-postgres` crate**, not a
first-party component. It exists to add two capabilities the upstream crate does
not expose — text-format results and a cap on the size of server messages — and
it is kept as close to upstream as possible so that pulling in a
new release stays a mechanical exercise.

| | |
| --- | --- |
| Upstream | <https://github.com/rust-postgres/rust-postgres> |
| Package | `tokio-postgres` |
| Version | `0.7.18` |
| Source | `https://static.crates.io/crates/tokio-postgres/tokio-postgres-0.7.18.crate` |
| `.crate` SHA-256 | `a528f7d280f6d5b9cd149635c8705b0dd049754bc67d81d31fa25169a93809d3` |
| License | MIT OR Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`) |

The tree is the **published crate tarball extracted verbatim**, with only the
edits catalogued below. The published package is used rather than a git checkout
because its sibling `path` dependencies are already rewritten to registry
versions, which keeps the diff to the nine edits and avoids vendoring
`postgres-protocol` and `postgres-types` as well.

To confirm provenance:

```sh
curl -sL -o tp.crate https://static.crates.io/crates/tokio-postgres/tokio-postgres-0.7.18.crate
sha256sum tp.crate   # must match the table above
```

## Why this fork exists

A general-purpose SQL tool built on the upstream crate returns an error instead of a
result whenever a column has a type the driver can't decode, such as `inet`:

```
error deserializing column 1: could not deserialize column "server_addr" (type inet)
```

tokio-postgres always asks PostgreSQL for results in the **binary** wire format.
That is a deliberate upstream design constraint, not an oversight: the `FromSql`
trait takes no format discriminator —

```rust
/// Creates a new value of this type from a buffer of data of the specified
/// Postgres `Type` in its binary format.
fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>>;
```

— so every `FromSql` implementation, in this crate and in every third-party
crate, is contractually a *binary* decoder. If the driver asked for text, all of
them would silently misparse. The driver must therefore pick one format for the
whole connection, and it picks binary. (Note the asymmetry: `ToSql` *does* have
`encode_format()` and `postgres-types` *does* ship a `Format { Text, Binary }`
enum — but only for outbound parameters.)

The consequence for a general-purpose SQL tool is that any type without a binary
decoder fails: most built-ins (`bytea`, `interval`, `time`, `money`, `xml`,
`tsvector`, ranges, enums, composites, `date[]`, `timestamptz[]`, the network
types) and **every** extension type (pgvector `vector`, PostGIS `geometry`,
`hstore`, `cube`, the `isn` family, …). Shipping a binary decoder per type is
unbounded work and can never cover an extension we have not anticipated.

Requesting the **text** format instead makes PostgreSQL render each value with
its own output function — exactly what `psql` receives — so any type
displays correctly, including ones this codebase has never heard of. The wire
protocol already supports this: `postgres_protocol::message::frontend::bind`
accepts `result_formats: impl IntoIterator<Item = i16>`. Only tokio-postgres's
hardcoded `Some(1)` stands in the way.

### Alternatives that were rejected

* **A `[patch]` onto an external fork repo** — same maintenance burden, plus a
  network dependency in the release build. Vendoring keeps the change auditable
  in-tree.
* **`simple_query`** — the simple protocol is always text, but its rows carry no
  type OIDs (`SimpleColumn` exposes only `name()`) and it has no portal, so both
  the `bool` fixup and the server-side row cap would be lost.
* **Rewriting user SQL to cast columns to text** — no fork, but it wraps the
  statement in a subquery (losing the formal row-order guarantee), needs
  `num_nulls()` guards to keep SQL `NULL` distinct from an empty string, and
  `::text` is *wrong* for the type in the original issue: PostgreSQL registers an
  explicit `text(inet)` cast, so `'192.168.1.1'::inet::text` yields
  `192.168.1.1/32` rather than `192.168.1.1`.
* **Migrating to `sqlx`** — its value model is format-aware and its unprepared
  path is text, but the prepared path hardcodes
  `result_formats: &[PgValueFormat::Binary]` in exactly the same way, so it would
  need the same patch, while also costing a full data-layer rewrite and the
  server-side row cap.

### Why the message size cap exists

The codec buffers each server message whole before handing it on, so one row
with a huge value — `SELECT repeat('x', 900000000)` — makes the client hold
the whole row, however small the caller's own limits on the result. The query
tool's 1 MiB CSV cap can only apply once the row has arrived. Edits 6–9 add an
optional `Config::max_backend_message_size`: the codec reads each message's
4-byte length header before buffering its body, and fails the connection when
the length is over the cap, so at most a read buffer's worth of the message is
held. `Client::backend_message_too_large` then tells the caller why the
connection closed, since pending requests see only `connection closed`.

## The patch — nine edits

Every edit is additive: every pre-existing caller passes `FORMAT_BINARY`, and
no message size cap is set unless a caller asks for one, so **default behavior
is identical to upstream**. All edits are greppable:

```sh
grep -rn "pghacker-mcp fork edit" Cargo.toml src/
```

| # | File | Item | Change |
| --- | --- | --- | --- |
| 1 | `Cargo.toml` | top of file | Add an empty `[workspace]` table. Required because this package sits inside the repository's workspace directory but is deliberately excluded from it. |
| 2 | `src/query.rs` | after the imports | Add `pub(crate) const FORMAT_BINARY: i16 = 1;` and `pub(crate) const FORMAT_TEXT: i16 = 0;`. |
| 3 | `src/query.rs` | `encode_bind_raw`, `encode_bind`, and their three internal callers | Thread a `result_format: i16` parameter through, and change the hardcoded `Some(1)` argument to `frontend::bind` into `Some(result_format)`. The three existing call sites inside `query.rs` pass `FORMAT_BINARY`. **This is the entire point of the fork.** |
| 4 | `src/bind.rs` | `bind` | Add `pub async fn bind_with_result_format(…, result_format: i16)` containing the original body; `bind` becomes a one-line delegation passing `query::FORMAT_BINARY`. |
| 5 | `src/transaction.rs` | after `bind_raw` | Add `Transaction::bind_text` and `Transaction::bind_raw_text`, mirroring the upstream `bind` / `bind_raw` pair but passing `query::FORMAT_TEXT`. |
| 6 | `src/config.rs` | `Config`, `Config::new`, after `get_load_balance_hosts`, `Debug` | Add a `max_backend_message_size: Option<usize>` field (default `None`), the `max_backend_message_size` setter and `get_max_backend_message_size` getter, and the field in `Debug`. |
| 7 | `src/codec.rs` | `PostgresCodec`, `decode` | `PostgresCodec` becomes a struct with `max_message_size` and a shared `message_too_large: Arc<AtomicBool>`. `decode` checks each header's length against the cap before buffering: messages already complete are returned first, then it sets the flag and returns an `InvalidData` error, which ends the connection. |
| 8 | `src/client.rs` | `Client`, `Client::new`, after `is_closed` | Add the shared flag to `Client` and a `Client::new` parameter for it, and `pub fn backend_message_too_large(&self) -> bool`. |
| 9 | `src/connect_raw.rs` | `connect_raw` | Build the codec from `config.max_backend_message_size` with a new flag, and pass the flag to `Client::new`. |

Nothing else in the crate is touched. No upstream behavior, type or public item
is removed or altered.

The machine-readable `fork-watch.json` mirrors the provenance table and records
the patch-marker and decisive-snippet counts.

### Contract for callers

A portal created with `bind_text` / `bind_raw_text` yields rows whose column
buffers hold **text**, so they must not be read with the ordinary typed
accessors — those assume binary and would misparse. Read them with a decoder
that expects text (see `RawText` in
`crates/pghacker-sqlquery/src/execute.rs`).

The result format lives in the `Bind` message while the row limit lives in
`Execute`, so `query_portal(&portal, max_rows)` keeps applying its server-side
cap exactly as it does for a binary portal.

## Updating to a new upstream release

1. Download the new published crate and verify its checksum against
   <https://crates.io>:
   ```sh
   V=<new-version>
   curl -sL -o tp.crate "https://static.crates.io/crates/tokio-postgres/tokio-postgres-$V.crate"
   sha256sum tp.crate
   ```
2. Extract it over this directory, replacing everything.
3. Re-apply the nine edits from the table above.
4. Verify the patch is actually in place — this must print exactly one line:
   ```sh
   grep -n "Some(result_format)" src/query.rs
   ```
   and this must print twenty-one markers (edits 6–9 mark every line they
   touch):
   ```sh
   grep -rn "pghacker-mcp fork edit" Cargo.toml src/ | wc -l   # => 21
   ```
5. Update the version, source URL and SHA-256 in the table at the top of this
   file, and the version in `../../Cargo.toml` if the major/minor changed.
6. Rebuild and run the regression suite, which fails loudly if results silently
   revert to the binary format:
   ```sh
   cd ../..
   cargo test --workspace --locked -- --test-threads=1
   ```
   The decisive test is `types_without_binary_decoders_render_as_text` in
   `crates/pghacker-sqlquery/tests/query.rs`, which pins exact renderings for
   `interval`, `inet`, `cidr`, `bytea`, `jsonb` and arrays.

Do **not** run `cargo fmt` over this directory. Preserving upstream formatting
byte-for-byte is what keeps step 3 a small, readable diff. The workspace
`exclude` in `../../Cargo.toml` already keeps `cargo fmt --all` and
`cargo clippy --workspace` out of here.

## Removing this fork

The fork can be deleted outright if upstream gains a way to choose the result
format and to cap message size. `frontend::bind` already accepts result format codes, so the upstream
change is small — an API addition along the lines of edit 5. If a PR to
`rust-postgres/rust-postgres` lands:

1. Drop the `[patch.crates-io]` and `exclude` entries from `../../Cargo.toml`.
2. Delete this directory.
3. Point `run_query_capture` in `crates/pghacker-sqlquery/src/execute.rs` and
   `QueryTool::new` in `crates/pghacker-sqlquery/src/query.rs` at the upstream
   APIs.
4. Remove the vendored copy's entry from `../../NOTICE`.
