# cfggraft-cli

Command line front end for `cfggraft`. Installs a binary named `cfggraft`.

```sh
cargo install cfggraft-cli
```

```
cfggraft merge   --key .env --fragment env.json --file ~/.config/app/settings.json --in-place
cfggraft region  --marker PROFILE --body generated.md --file ~/.config/app/notes.md --in-place
```

This crate is argument parsing, file I/O, reporting, and the exit status. Every
decision about what to change is made by the library.

| exit | meaning |
|---|---|
| 0 | applied, or already up to date |
| 1 | failure — bad input, unreadable file, missing marker |
| 2 | drift — a difference it declined to overwrite. Nothing was written. |

See the repository README for the model and the flags.
