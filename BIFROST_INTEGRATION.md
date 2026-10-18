# Bifrost integration

The Bifrost project owns the portable bridge and the Hulder adapter. Hulder embeds a source snapshot in `crates/bifrost-core` and `crates/bifrost`. The CLI MCP module delegates to that core. `hulder mcp` retains its existing invocation and configured socket selection.

The snapshot is recorded in `bifrost-source.json`. Update it from a Bifrost checkout using its `scripts/sync-hulder.py` command. A normal Hulder checkout does not require that separate checkout to build.

Run the following from the Bifrost checkout:

```sh
python3 scripts/sync-hulder.py /path/to/Hulder --write
python3 scripts/sync-hulder.py /path/to/Hulder
```

Then run `cargo check -p bifrost -p cli` and `cargo test -p bifrost` inside Hulder. Review both repository diffs. Preserve the embedded GPL license and attribution files. Do not edit embedded files independently of the Bifrost source.
