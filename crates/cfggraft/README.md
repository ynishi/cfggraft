# cfggraft

Inject declared items into config files that belong to another program.

One rule: **apply what is provably ours, report what is not, never overwrite a
disagreement.** Deciding that needs three values — what is declared, what this
library wrote last time, and what is in the target now — so a value it wrote
itself can be updated or withdrawn, while a value the other program wrote is
left alone and reported.

The unit is an item, not a file format. `policy` decides, `port` states the
interface a representation implements, `adapter` implements it for JSON
documents and marker-delimited spans, `engine` walks the items, and `prior`
persists provenance. Adding a representation means adding an adapter; no other
module changes.

The whole design, including the decision table, is in the crate documentation:

```sh
cargo doc --open
```

The command line front end is `cfggraft-cli`, which installs a binary named `cfggraft`.
