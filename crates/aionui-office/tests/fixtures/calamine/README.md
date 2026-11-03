Test fixtures copied unchanged from [tafia/calamine](https://github.com/tafia/calamine)
v0.38.0, commit `a4dba3b5309d81b2774018a13be39e68d938c3c7`.

- Source: `tests/issues.{xls,xlsb,xlsx}`, `tests/cfb_dir_short_chunk_panic.xls`,
  and `tests/cfb_difat_oom.xls` at that commit.
- Expected `issue2` cells and CFB regression behavior come from the upstream
  `tests/test.rs` tests for those files.
- License: MIT; the upstream `LICENSE-MIT.md` is included verbatim.
- These files are only parsed as test input. No macro execution is part of the tests.

The CFB cases exercise the short-directory-chunk and cyclic-DIFAT regressions
fixed by calamine 0.38.0 (RUSTSEC-2026-0436). The ordinary workbooks exercise
`ConversionService` through XLS, XLSB and XLSX readers.

| File | SHA-256 |
| --- | --- |
| `cfb_difat_oom.xls` | `b0ff76967dfbb4e4870c848a6365c74f361e48bfd98afed1a5ed378b0ab2a6a8` |
| `cfb_dir_short_chunk_panic.xls` | `82de55b1a1c0ee69e8460e29172af5711a109f7ad04a75f7d112e938fe3ec9c7` |
| `issues.xls` | `5e7f206dd7a8a9b86637f65c02fb85e9dad2a52ec5bf0b92e6eef1746c55b301` |
| `issues.xlsb` | `06c648de5529e022ef399a2b6ebc227040e9fdf71a0780e19e614f556bdb53d7` |
| `issues.xlsx` | `f41bbd13cb3f9648b2492bc7ffce4c0544a27d0c62daec5c670ac15bdfe04bbc` |
