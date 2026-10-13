# grafile on-disk format

Version 0.1 (format major 0, minor 1). All multi-byte integers are
little-endian unless stated otherwise. The database is a single file; a
transient `-journal` sidecar exists only while a write transaction is in
flight (see [Transactions](#transactions)).

## Pages

The file is an array of fixed-size pages. Page numbers are 1-based; page 0 is
invalid. The page size is chosen at database creation (power of two,
4096..=65536, default 8192) and never changes.

Every page starts with a 20-byte page header:

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 1 | page type |
| 1 | 1 | flags (reserved, must be 0) |
| 2 | 2 | cell count |
| 4 | 2 | content start (offset of the cell content area) |
| 6 | 2 | fragmented free bytes |
| 8 | 4 | page number (self, for sanity checks) |
| 12 | 4 | rightmost child (interior pages only, else 0) |
| 16 | 4 | checksum: CRC32C over bytes `[0..16)` and `[20..page_size)` |

Page types:

| Value | Type |
|------:|------|
| 0x00 | header page (page 1 only) |
| 0x01 | table B+tree interior |
| 0x02 | table B+tree leaf |
| 0x03 | index B+tree interior |
| 0x04 | index B+tree leaf |
| 0x05 | overflow |
| 0x06 | freelist trunk |
| 0x07 | freelist leaf |

## Database header (page 1)

Page 1 is reserved for metadata; it holds no B+tree cells. The header begins
at offset 20 and is 128 bytes long:

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 8 | magic `GRAFILE\0` |
| 8 | 2 | format major |
| 10 | 2 | format minor |
| 12 | 4 | page size (power of two) |
| 16 | 1 | reserved bytes per page (0 in this version) |
| 17 | 1 | text encoding (0 = UTF-8) |
| 18 | 2 | padding (0) |
| 20 | 4 | flags (reserved, must be 0) |
| 24 | 8 | page count (highest allocated page number) |
| 32 | 4 | freelist trunk page (0 = empty) |
| 36 | 4 | freelist page count |
| 40 | 4 | catalog B+tree root |
| 44 | 4 | node table B+tree root |
| 48 | 4 | edge table B+tree root |
| 52 | 4 | fragment store B+tree root |
| 56 | 8 | next node id (monotonic, never reused) |
| 64 | 8 | next edge id (monotonic, never reused) |
| 72 | 4 | next label id (starts at 1) |
| 76 | 4 | next edge type id (starts at 1) |
| 80 | 4 | next property key id (starts at 1) |
| 84 | 4 | change counter (incremented once per committed write txn) |
| 88 | 4 | schema generation (incremented by DDL) |
| 92 | 8 | creation time (Unix seconds) |
| 100 | 16 | database UUID |
| 116 | 4 | user version (application-managed) |
| 120 | 4 | application id (application-managed) |
| 124 | 4 | header checksum: CRC32C over bytes `[0..124)` |
Id 0 is reserved as a sentinel (invalid node/edge, "any" label/type filter).
All registries therefore start allocating at 1.

## Cell formats

Cells are stored in the content area, which grows downward from
`page_size - reserved_bytes`; the cell pointer array grows upward from offset
20. Free bytes = `content_start - (20 + 2 * cell_count)`. The
`fragmented_free` counter tracks bytes lost to deletion.

### Table B+tree

Keys are 8-byte big-endian `u64` ids, so byte order equals numeric order.

Table interior cell (12 bytes):

| Size | Field |
|-----:|-------|
| 4 | left child page |
| 8 | separator key (first key of the child's subtree) |

The rightmost child lives in the page header. Interior pages carry copies of
leaf keys; search descends via "last key <= target", and the rightmost child
covers keys greater than every separator.

Table leaf cell:

| Size | Field |
|-----:|-------|
| 8 | key |
| 4 | payload length; bit 31 set = overflow |
| 2 | local payload length (overflow only) |
| 4 | first overflow page (overflow only) |
| * | local payload bytes |

Non-overflow cells store `[key][len][payload]`. Overflowing records store the
first `local` bytes inline plus a chain of overflow pages.

### Index B+tree

Keys are arbitrary byte strings compared with `memcmp` (the record encoder is
order-preserving). Values are payloads.

Index interior cell: `[child u32][key_len u16][key bytes]`, plus the rightmost
child in the page header.

Index leaf cell: `[key_len u16][key bytes][value payload]` where the value
payload uses the same length/overflow encoding as table leaves.

### Overflow pages

| Offset | Size | Field |
|-------:|-----:|-------|
| 20 | 4 | next overflow page (0 = end) |
| 24 | * | data |

Usable data per overflow page = `page_size - 24`.

## Fragment store

The fragment B+tree (index tree) maps 13-byte keys to serialized fragments:

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 1 | family |
| 1 | 4 | family id |
| 5 | 4 | band 0 |
| 9 | 4 | band 1 |

Families:

| Family | Meaning | family id | band 0 | band 1 | bit position |
|-------:|---------|-----------|--------|--------|--------------|
| 0 | incidence, out direction | edge type (0 = any) | node id >> 8 | edge id >> 8 | `(node id & 255) * 256 + (edge id & 255)` |
| 1 | incidence, in direction | edge type (0 = any) | node id >> 8 | edge id >> 8 | same |
| 2 | label membership | label id | node id >> 16 | 0 | `node id & 65535` |
| 3 | edge type membership | edge type | edge id >> 16 | 0 | `edge id & 65535` |

A fragment is a 65536-bit bitmap. Missing key = all-zero fragment. Encodings:

| Byte | Encoding |
|-----:|----------|
| 0 | sparse: `u16` little-endian set-bit offsets, ascending |
| 1 | dense: 8192 bytes, little-endian `u64` words |

Fragments are derived data: they can always be rebuilt from node and edge
records. Dense fragments exceed a B+tree leaf cell, so they overflow.

## Record encoding

Values are encoded with a 1-byte tag followed by payload:

| Tag | Type | Payload |
|----:|------|---------|
| 1 | false | none |
| 2 | true | none |
| 3 | integer | zigzag LEB128 varint |
| 4 | float | 8-byte IEEE 754 big-endian |
| 5 | text | varint length + UTF-8 bytes |
| 6 | bytes | varint length + raw bytes |
| 7 | list | varint count + encoded elements |

Null properties are not stored (Cypher treats them as absent).

Node record: `[label count varint][label ids as delta varints][property count varint][(key id varint, value)*]`, labels and property keys sorted ascending.

Edge record: `[edge type varint][src varint][dst varint][property count varint][(key id varint, value)*]`.

Index keys are order-preserving: type tag, then for integers the sign-flipped
big-endian u64, for floats the sign-flipped IEEE bits (negative values
bitwise-negated), for text the escaped bytes with `0x00` mapped to
`0x00 0xFF` and terminated by `0x00 0x01`, for null just the tag. Composite
keys concatenate encoded fields.

## Freelist

Freelist trunk pages: `[page header][next trunk u32][count u32][page numbers u32 * count]`,
capacity `(page_size - 28) / 4` entries. Freelist leaf pages have no content.
The header's `freelist_trunk` and `freelist_count` fields describe the list.

## Transactions

grafile uses a rollback journal (SQLite "exclusive" style). The journal path is
`<db>-journal`.

Journal header (first page-sized block):

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 8 | magic `GRAJNL1\0` (zeroed on clean shutdown of a txn) |
| 8 | 4 | page size |
| 12 | 4 | database change counter before the transaction |
| 16 | 8 | random salt |
| 24 | 8 | record count |
| 32 | 4 | header checksum: CRC32C over bytes `[0..32)` |

Each record: `[page number u32][original page bytes][CRC32C over both]`.

Commit protocol:

1. Acquire an exclusive lock on the database file.
2. Append original page images to the journal as pages are first modified.
3. Write the journal header (magic, record count, salt, checksum); `sync_all`.
4. Write dirty database pages in ascending page order; `sync_all`.
5. Zero the journal magic; `sync_all`; remove the journal file.
6. Release the lock.

Crash recovery, performed under an exclusive lock on open:

- No journal, or journal header magic zero/invalid: delete the journal if
  writable; the database is consistent.
- Valid journal header: restore each recorded page image, stopping at the
  first record whose checksum fails; then delete the journal.

Because database pages are only written after a valid, synced journal header,
a torn journal can never correspond to a partially written database.

### Multi-connection behavior

- Readers take a shared file lock for the duration of each statement. On
  acquiring it they replay a hot journal left by a crashed writer and drop
  cached pages when the header's change counter has moved, so a connection
  never observes stale or partially committed state.
- Writers take the exclusive file lock plus a process-global writer slot
  keyed by the file path: advisory `fcntl` locks are per-process, so they
  cannot exclude two handles inside the same process on their own.
- A handle dropped mid-transaction releases its writer slot; the next writer
  or reader recovers the abandoned journal.

## Growth and durability

- The file grows one page at a time when the freelist is empty.
- Every page carries a CRC32C; reads fail with `Corrupt` on mismatch.
  Checksums are computed once per page when it is written at commit, not on
  every in-memory mutation.
- Committed transactions are durable: the journal is flushed with a full
  sync (drive-cache flush on macOS), database pages and the journal
  invalidation use a normal `fsync`, mirroring SQLite's `SQLITE_SYNC_FULL`
  for the journal and `SQLITE_SYNC_NORMAL` for the rest.
