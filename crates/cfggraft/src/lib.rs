//! Inject declared items into config files owned by another program.
//!
//! A config file you want to manage from a repository is often not yours alone.
//! The application writes to it too — a model choice, a remembered permission, a
//! UI toggle. Owning the whole file (symlink, template render, `cp`) throws
//! those away. Owning nothing means the repository cannot configure anything.
//!
//! So this crate injects *items* and leaves everything else alone. One rule
//! covers every operation: **apply what is provably ours, report what is not,
//! never overwrite a disagreement.**
//!
//! # The unit is an item, not a file format
//!
//! Managing config from above means declaring, per item, what its value should
//! be. Whether that item lives at a key path in a JSON document, inside a
//! marker-delimited span of a text file, or as a file in a directory is a
//! question of *representation* — how the item is addressed, read, and written.
//! It does not change what the management decision is.
//!
//! That split is the whole architecture:
//!
//! - [`policy`] decides. One table, no knowledge of any format.
//! - [`port::Store`] is the interface a representation implements.
//! - [`adapter`] holds the implementations ([JSON documents][adapter::json],
//!   [marker-delimited spans][adapter::marker]).
//! - [`engine`] walks the items and applies the decisions.
//! - [`prior`] persists provenance for representations that cannot hold it
//!   inline.
//!
//! Adding a representation means adding an adapter. No other module changes.
//!
//! # Three inputs, not two
//!
//! The central invariant: **a decision needs three values, not two.**
//!
//! | | |
//! |---|---|
//! | `declared` | what the repository says the value should be |
//! | `prior` | what this tool wrote on its last run |
//! | `observed` | what is in the target right now |
//!
//! Comparing only `declared` against `observed` cannot answer the question that
//! matters, which is *who wrote the value that is there*. Without `prior`, a
//! value this tool wrote itself is indistinguishable from a value the other
//! program wrote, so every change to a declaration looks like a conflict and no
//! value can ever be updated or withdrawn.
//!
//! `prior` collapses that ambiguity into [`port::Ownership`], which is what the
//! policy actually branches on. The same three-way shape is what Terraform
//! compares (configuration, state, remote) and what Kubernetes server-side apply
//! records per field manager.
//!
//! # The contract
//!
//! [`policy::decide`] is total over this table and nothing else decides
//! anything:
//!
//! | declared | ownership | observed | decision | why |
//! |---|---|---|---|---|
//! | yes | `Unknown` | absent | `Add` | first run |
//! | yes | `Unknown` | equal | `Skip` | already agreed; ownership is recorded, adopting the item |
//! | yes | `Unknown` | differs | `Refuse(Conflict)` | ownership cannot be claimed, so the value stays |
//! | yes | `Ours` | equal | `Skip` | already correct |
//! | yes | `Ours` | differs | `Update` | the old value is provably ours |
//! | yes | `Diverged` | any | `Refuse(HandEdited)` | someone changed what we wrote |
//! | yes | `Removed` | absent | `Refuse(RemovedByOther)` | the deletion was deliberate; do not resurrect it |
//! | no | `Ours` | present | `Retract` | ours, and no longer declared |
//! | no | other | any | `Skip` | not ours |
//!
//! Two properties follow, and both are relied on elsewhere:
//!
//! - **Nothing is written or removed unless ownership is `Ours`**, except `Add`,
//!   which writes only where there was nothing.
//! - **Re-running with an unchanged declaration produces only `Skip`.** Every
//!   decision that changes the target moves ownership to `Ours` with the
//!   declared value, which is the `Skip` row on the next run.
//!
//! # Provenance lives wherever the representation allows
//!
//! Ownership requires remembering what was written. Where that record lives is
//! the adapter's choice, because it depends on what the target can carry:
//!
//! | adapter | provenance |
//! |---|---|
//! | [`adapter::marker`] | inline — a digest in the begin marker |
//! | [`adapter::json`] | side-car, via [`prior`] |
//!
//! Text can hold a comment, so a marker-delimited region is self-describing and
//! needs no external file. JSON cannot hold a comment, and adding a reserved key
//! to another program's document would be a modification that program never
//! asked for and might reject. So JSON provenance goes to a side-car file that
//! belongs to this tool.
//!
//! Provenance is local to the machine holding the target, never to the
//! repository holding the declaration: it records what was written *here*, and
//! two machines will legitimately differ.
//!
//! **When provenance is unavailable** — a lost side-car, a target read from
//! stdin — every item reads as [`port::Ownership::Unknown`]. The table above
//! then permits only `Add` and `Skip`, so the tool degrades to add-only. That is
//! a loss of capability, never a loss of data.
//!
//! # Values cross the port as text
//!
//! Items carry a [`port::Repr`]: the value in a canonical textual form. The core
//! compares and displays reprs and never inspects their structure.
//!
//! This is deliberately *not* a shared intermediate representation. Each adapter
//! canonicalises into its own dialect and parses back from it, so a TOML value
//! would round-trip through TOML text rather than through JSON. Routing every
//! format through one structural type would silently drop what that type cannot
//! hold — comments, key order, and the distinctions between a TOML datetime, an
//! integer, and a float — which is precisely the data this crate exists to
//! preserve.
//!
//! # Example
//!
//! ```
//! use cfggraft::policy::{decide, Decision};
//! use cfggraft::port::{Observed, Ownership, Repr, State};
//!
//! // A value this tool wrote last run, with the declaration since changed.
//! let state = State::new(Observed::Present(Repr::new("\"1\"")), Ownership::Ours);
//! assert_eq!(decide(Some(&Repr::new("\"2\"")), &state), Decision::Update);
//!
//! // The same observation without provenance is not ours to touch.
//! let state = State::new(Observed::Present(Repr::new("\"1\"")), Ownership::Unknown);
//! assert!(matches!(decide(Some(&Repr::new("\"2\"")), &state), Decision::Refuse(_)));
//! ```
//!
//! # Adding an adapter
//!
//! Implement [`port::Store`] and add a variant to [`adapter::Adapter`]. The
//! trait is six methods: expand the declaration into items, list what was owned,
//! report an item's state, write, retract, and finish. Decisions, reporting,
//! exit codes, and the `--force` and `--retract` policies are already handled.
//!
//! Dispatch is an enum rather than a trait object because the set of adapters is
//! closed within this crate; the trait exists to state the contract an adapter
//! must meet.

// This crate's contract *is* its documentation, so an undocumented public item
// is an unstated contract rather than a missing nicety.
#![warn(missing_docs)]

pub mod adapter;
pub mod atomic;
pub mod engine;
pub mod error;
pub mod policy;
pub mod port;
pub mod prior;

pub use error::{Error, Result};
