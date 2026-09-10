# Vendored crates

`aether-data` and `aether-data-derive` are copied from
[iamacoffeepot/aether](https://github.com/iamacoffeepot/aether) at commit
`7e623b642dc0d5040eec704ce78853132045c891` (`crates/aether-data`,
`crates/aether-data-derive`). They provide the ADR-0059 storage shape
(`#[derive(Storage)]`, content-hashed field tags, unknown-field bucket) that
the journal's event kinds are written in.

They are vendored rather than depended on because aether is not published to
crates.io and a git dependency would clone the whole aether repository on every
first build. The copy is verbatim apart from each crate's `Cargo.toml`, which
was rewritten with concrete dependency versions in place of aether's workspace
inheritance, and the derive crate's `tests/` directory, which was dropped.

Both crates are licensed `MIT OR Apache-2.0` upstream.

To refresh, re-copy the two `src/` trees from the aether commit you want and
update the hash above. The journal's on-disk format depends on
`aether-data/src/storage`; a refresh that changes that directory needs a
journal-compatibility check (`scry --journal-dump` over an existing journal)
before it ships.
