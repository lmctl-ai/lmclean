# Standard `lmcode` REST Endpoints & Stateless CLI Interfaces

## Summary

`lmclean` upgrades `dslvm` (`lmcode-rust`) to the 226-signature `lmcode_std_v3` standard catalog and `@lmctl-ai/lmctl@0.2.482`:
- Supports `RestGet`..`RestOptions`, `RestHandler`, `CliCommand`, `CliOption`, `rest_config`, `rest_route`, `rest_serve`, and `rest_cli`.
- In `src/main.rs`, when a program defines only REST/CLI entries (`Rest*` / `Cli*`) and no filesystem walk SAM rules (`Ignore`/`Select`/`Report`), `lmclean` executes the top-level body (including `rest_cli` or `rest_serve`) without forcing a filesystem walk.
- Verified in `test.sh` (section 12b) and `cargo test`.
