//! The interface a representation implements.
//!
//! A [`Store`] is one target document seen as a set of addressable items. It
//! answers what an item's state is, writes an item, retracts one, and renders
//! the result. Everything about *which* change to make is decided by
//! [`crate::policy`] and driven by [`crate::engine`]; a store never consults the
//! declaration to choose an outcome.
//!
//! Splitting it this way is what keeps the management rules in one place. Two
//! representations as unalike as a JSON document and a marker-delimited span
//! differ only in how they address, read, and write items — not in what should
//! happen when an item is already present.

use crate::error::Result;
use std::fmt;

/// Where an item lives, in whatever notation its representation uses.
///
/// The core treats an address as opaque: it compares addresses for identity and
/// prints them in reports, and never parses one. A JSON document uses dotted key
/// paths (`env.EDITOR`), a marker region uses the marker name (`PROFILE`), and a
/// future representation may use anything it can render and compare.
///
/// Addresses appear in provenance records, so an address must denote the same
/// item across runs. An index into a sequence does not qualify — inserting an
/// element renumbers its neighbours — which is why the JSON adapter addresses
/// set elements by value.
///
/// ```
/// use cfggraft::port::Addr;
///
/// let a = Addr::new("env.EDITOR");
/// assert_eq!(a.to_string(), "env.EDITOR");
/// assert_eq!(a, Addr::new("env.EDITOR"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Addr(String);

impl Addr {
    /// Build an address from its rendered form.
    pub fn new(s: impl Into<String>) -> Self {
        Addr(s.into())
    }

    /// The rendered form, as stored in provenance.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An item's value in canonical textual form.
///
/// The core compares reprs for equality and truncates them for display. It never
/// inspects their structure, so each adapter is free to choose its own dialect
/// and is responsible for two properties:
///
/// - **Canonical.** Two values that the representation considers equal produce
///   the same repr. The JSON adapter sorts object keys for this reason: the
///   other program may rewrite a document with its keys in a different order,
///   and that is not a change to the value.
/// - **Round-trippable.** The adapter can recover a value it can write from a
///   repr it produced. This is what lets a retraction find, in the target, the
///   element recorded in provenance.
///
/// Because every repr stays inside the adapter that made it, formats never share
/// a structural type and so never lose what that type cannot hold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Repr(String);

impl Repr {
    /// Wrap an already-canonical rendering.
    pub fn new(s: impl Into<String>) -> Self {
        Repr(s.into())
    }

    /// The canonical text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The text, shortened for a one-line report.
    ///
    /// ```
    /// use cfggraft::port::Repr;
    ///
    /// assert_eq!(Repr::new("short").truncated(10), "short");
    /// assert_eq!(Repr::new("0123456789abc").truncated(10), "0123456789…");
    /// ```
    pub fn truncated(&self, max: usize) -> String {
        if self.0.chars().count() > max {
            let head: String = self.0.chars().take(max).collect();
            format!("{head}…")
        } else {
            self.0.clone()
        }
    }
}

impl fmt::Display for Repr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One declared item: an address and the value it should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Where the item lives.
    pub addr: Addr,
    /// The canonical form of its value.
    pub repr: Repr,
}

impl Item {
    /// Pair an address with a value.
    pub fn new(addr: Addr, repr: Repr) -> Self {
        Item { addr, repr }
    }
}

/// What is in the target right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// No item at this address.
    Absent,
    /// An item holding this value.
    Present(Repr),
}

impl Observed {
    /// Whether the observed value equals `repr`.
    pub fn equals(&self, repr: &Repr) -> bool {
        matches!(self, Observed::Present(o) if o == repr)
    }
}

/// How what is in the target relates to what this tool last wrote there.
///
/// This is the adapter's answer to "is this value ours?", and the only thing the
/// policy branches on. How it is determined is the adapter's business: a marker
/// region compares a digest recorded in the file itself, a JSON document
/// compares against a side-car record. Both reduce to the same four cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Nothing was recorded for this address. Either this is the first run, or
    /// provenance is unavailable. Ownership cannot be claimed, so the policy
    /// permits only adding what is absent and agreeing with what matches.
    Unknown,
    /// The value present is exactly what this tool last wrote. It is ours to
    /// update or retract.
    Ours,
    /// Something was recorded, and what is there now differs from it. Another
    /// writer changed it, so it is no longer ours.
    Diverged,
    /// Something was recorded, and the item is now gone. Another writer deleted
    /// it, and a deletion is as deliberate as any other edit.
    Removed,
}

/// An item's state: what is there, and whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// What the target holds at this address.
    pub observed: Observed,
    /// How that relates to what this tool last wrote.
    pub ownership: Ownership,
    /// Adapter-supplied detail for human-readable reports, such as the digests
    /// behind a [`Ownership::Diverged`] verdict.
    ///
    /// For display only. The core never branches on it, so an adapter that has
    /// nothing useful to say may leave it empty without changing any outcome.
    pub note: Option<String>,
}

impl State {
    /// A state with no reporting detail.
    pub fn new(observed: Observed, ownership: Ownership) -> Self {
        State {
            observed,
            ownership,
            note: None,
        }
    }

    /// Attach reporting detail.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The target document, rendered in full.
    ///
    /// This is always a complete document, including when nothing changed and
    /// when a change was refused. A caller that writes it unconditionally still
    /// cannot destroy anything.
    pub document: String,
    /// Provenance to persist outside the document, for adapters that keep it
    /// there. An adapter recording provenance inline returns `None` — the record
    /// is already in `document`.
    pub prior: Option<Vec<Item>>,
}

/// One target document, seen as a set of addressable items.
///
/// Implementing this trait is the whole of adding a representation. The six
/// methods divide into three pairs: what is declared and what was owned, how one
/// item reads, and how one item is written or withdrawn.
///
/// # Contract
///
/// - [`declared`](Store::declared) and [`owned`](Store::owned) return items
///   whose addresses are meaningful to [`state`](Store::state).
/// - [`state`](Store::state) is read-only and may be called for any address from
///   either list.
/// - [`write`](Store::write) is called only for an address present in
///   [`declared`](Store::declared), and writes that declared value. The core
///   never supplies a value of its own, so it cannot invent one.
/// - [`retract`](Store::retract) is called only for an address present in
///   [`owned`](Store::owned) whose ownership is [`Ownership::Ours`], and removes
///   the value recorded there.
/// - [`finish`](Store::finish) renders the document and returns the provenance
///   to persist.
///
/// An implementation that upholds these is correct regardless of what the
/// representation is, because every decision about *whether* to call them has
/// already been made.
pub trait Store {
    /// The items the declaration asks for, in declaration order.
    fn declared(&self) -> Result<Vec<Item>>;

    /// The items this tool wrote on its last run, as recorded in provenance.
    ///
    /// Empty when provenance is unavailable, which leaves every item
    /// [`Ownership::Unknown`] and reduces the run to add-only.
    fn owned(&self) -> Vec<Item>;

    /// What is at `addr`, and whose it is.
    fn state(&self, addr: &Addr) -> Result<State>;

    /// Set `addr` to its declared value.
    fn write(&mut self, addr: &Addr) -> Result<()>;

    /// Remove the value this tool recorded at `addr`.
    fn retract(&mut self, addr: &Addr) -> Result<()>;

    /// Render the document and hand back the provenance to persist.
    ///
    /// `owned` is the set of items this tool now claims, which the caller has
    /// computed from the decisions it applied.
    fn finish(&mut self, owned: &[Item]) -> Result<Output>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_equality_is_by_repr() {
        let o = Observed::Present(Repr::new("\"1\""));
        assert!(o.equals(&Repr::new("\"1\"")));
        assert!(!o.equals(&Repr::new("\"2\"")));
        assert!(!Observed::Absent.equals(&Repr::new("\"1\"")));
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let r = Repr::new("ありがとうございます");
        assert_eq!(r.truncated(3), "ありが…");
        assert_eq!(r.truncated(50), "ありがとうございます");
    }

    #[test]
    fn addresses_order_stably_for_provenance() {
        let mut v = vec![Addr::new("b"), Addr::new("a")];
        v.sort();
        assert_eq!(v, vec![Addr::new("a"), Addr::new("b")]);
    }
}
