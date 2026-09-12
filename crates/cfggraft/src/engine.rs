//! Walks the items and applies the decisions.
//!
//! [`run`] is the whole execution path: gather the declared items and the ones
//! provenance says were written last time, ask [`crate::policy::decide`] about
//! each, apply the result through the [`Store`], and hand back a [`Report`] and
//! the rendered document.
//!
//! Two caller-facing overrides live here rather than in the policy, so that the
//! decision table stays a statement of what is safe rather than a mixture of
//! that and what the invoker asked for. [`run`] applies them *after* a decision
//! is made, which also means a report can say plainly that a refusal was
//! overridden.

use crate::error::Result;
use crate::policy::{decide, Decision};
use crate::port::{Addr, Item, Observed, Output, Repr, State, Store};
use std::collections::BTreeMap;

/// Caller overrides applied on top of the decision table.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Overwrite values this tool cannot claim, turning every refusal into a
    /// write. This discards whatever the other writer put there, which is why it
    /// is never the default.
    pub force: bool,
    /// Remove items this tool wrote that the declaration no longer mentions.
    ///
    /// Off by default. Retraction only ever touches values whose ownership is
    /// [`Ours`](crate::port::Ownership::Ours), so it cannot reach another
    /// program's data — but it does mean deleting a line from the declaration
    /// deletes a value from the target, and that is a different promise from the
    /// add-only one made when it is off.
    pub retract: bool,
}

/// What happened to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The item's address.
    pub addr: Addr,
    /// What was decided, after any [`Options`] override.
    pub decision: Decision,
    /// The declared value, absent for an item that only provenance knows about.
    pub declared: Option<Repr>,
    /// What was in the target.
    pub observed: Observed,
    /// Adapter detail for reporting, such as the digests behind a hand-edit
    /// verdict.
    pub note: Option<String>,
    /// Whether an override changed this outcome from what the table gave.
    pub overridden: bool,
}

/// What a whole run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One entry per item considered, in the order they were walked.
    pub entries: Vec<Entry>,
}

impl Report {
    /// Whether anything about the target changed.
    pub fn changed(&self) -> bool {
        self.entries.iter().any(|e| e.decision.changes_target())
    }

    /// Whether any difference was left in place rather than overwritten.
    ///
    /// This is what an exit code of 2 reports, and it is deliberately narrower
    /// than "something went wrong": a failure to parse or read is an
    /// [`Error`](crate::Error), never drift. A check that cannot tell a
    /// difference from a crash reports nothing useful.
    pub fn has_drift(&self) -> bool {
        self.entries.iter().any(|e| e.decision.is_drift())
    }

    /// Entries whose decision matches a predicate.
    pub fn matching(&self, f: impl Fn(Decision) -> bool) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| f(e.decision))
    }

    /// How many items got each outcome, for a summary line.
    pub fn tally(&self) -> Tally {
        let mut t = Tally::default();
        for e in &self.entries {
            match e.decision {
                Decision::Add => t.added += 1,
                Decision::Skip => t.skipped += 1,
                Decision::Update => t.updated += 1,
                Decision::Retract => t.retracted += 1,
                Decision::Refuse(_) => t.refused += 1,
            }
        }
        t
    }
}

/// Counts per outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Items written where nothing was.
    pub added: usize,
    /// Items already in the declared state.
    pub skipped: usize,
    /// Items whose previous value was ours and was replaced.
    pub updated: usize,
    /// Items withdrawn.
    pub retracted: usize,
    /// Differences left in place.
    pub refused: usize,
}

/// Apply a declaration to a target.
///
/// Items are walked in declaration order, then any addresses known only to
/// provenance, sorted so a run is reproducible.
///
/// The target is mutated in memory only; persisting [`Output`] is the caller's
/// decision, which is what makes a read-only check identical to a real run in
/// everything but the final write.
///
/// # Ownership bookkeeping
///
/// The returned provenance claims exactly the items this tool now stands behind:
///
/// - applied and agreed items are claimed with their declared value — including
///   a [`Skip`](Decision::Skip) over an equal value, which adopts an item
///   written before provenance existed;
/// - a refusal over a value that was ours keeps the *old* record, so the next
///   run can still say precisely that the value was hand-edited rather than
///   losing that history and calling it an ordinary conflict;
/// - a refused conflict over a value that was never ours claims nothing;
/// - a retracted item is dropped, and a retraction suppressed by [`Options`]
///   keeps its claim so that enabling retraction later still works.
pub fn run<S: Store>(store: &mut S, opts: Options) -> Result<(Report, Output)> {
    let declared = store.declared()?;
    let prior = store.owned();

    let declared_by_addr: BTreeMap<&Addr, &Repr> =
        declared.iter().map(|i| (&i.addr, &i.repr)).collect();
    let prior_by_addr: BTreeMap<&Addr, &Repr> = prior.iter().map(|i| (&i.addr, &i.repr)).collect();

    let mut walk: Vec<&Addr> = declared.iter().map(|i| &i.addr).collect();
    let mut extra: Vec<&Addr> = prior_by_addr
        .keys()
        .copied()
        .filter(|a| !declared_by_addr.contains_key(a))
        .collect();
    extra.sort();
    walk.extend(extra);

    let mut report = Report::default();
    let mut owned: Vec<Item> = Vec::new();

    for addr in walk {
        let declared_repr = declared_by_addr.get(addr).copied();
        let state = store.state(addr)?;
        let base = decide(declared_repr, &state);
        let (decision, overridden) = override_decision(base, &state, declared_repr.is_some(), opts);

        match decision {
            Decision::Add | Decision::Update => store.write(addr)?,
            Decision::Retract => store.retract(addr)?,
            Decision::Skip | Decision::Refuse(_) => {}
        }

        claim(
            &mut owned,
            addr,
            decision,
            base,
            declared_repr,
            prior_by_addr.get(addr).copied(),
        );

        report.entries.push(Entry {
            addr: addr.clone(),
            decision,
            declared: declared_repr.cloned(),
            observed: state.observed,
            note: state.note,
            overridden,
        });
    }

    let output = store.finish(&owned)?;
    Ok((report, output))
}

/// Apply the caller's overrides to a decision, reporting whether it moved.
fn override_decision(
    base: Decision,
    state: &State,
    is_declared: bool,
    opts: Options,
) -> (Decision, bool) {
    match base {
        Decision::Refuse(_) if opts.force && is_declared => {
            let forced = match state.observed {
                Observed::Absent => Decision::Add,
                Observed::Present(_) => Decision::Update,
            };
            (forced, true)
        }
        Decision::Retract if !opts.retract => (Decision::Skip, true),
        other => (other, false),
    }
}

/// Record what this tool now claims for `addr`.
///
/// `base` is the decision before overrides, which is what distinguishes a
/// genuine [`Skip`](Decision::Skip) from a retraction the caller suppressed.
fn claim(
    owned: &mut Vec<Item>,
    addr: &Addr,
    decision: Decision,
    base: Decision,
    declared: Option<&Repr>,
    prior: Option<&Repr>,
) {
    let keep_prior = || prior.map(|r| Item::new(addr.clone(), r.clone()));

    let item = match (declared, decision) {
        // Applied or agreed: claim the declared value.
        (Some(d), Decision::Add | Decision::Update | Decision::Skip) => {
            Some(Item::new(addr.clone(), d.clone()))
        }
        // Refused over something that was ours: keep the old record so the next
        // run can still name what happened.
        (Some(_), Decision::Refuse(_)) => keep_prior(),
        // Suppressed retraction: still ours, still claimed.
        (None, Decision::Skip) if base == Decision::Retract => keep_prior(),
        // Withdrawn, or never ours.
        _ => None,
    };

    if let Some(item) = item {
        owned.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Reason;
    use crate::port::{Ownership, State};

    /// A store over a flat map, standing in for any representation.
    struct FakeStore {
        declared: Vec<Item>,
        prior: Vec<Item>,
        present: BTreeMap<Addr, Repr>,
        writes: Vec<Addr>,
        retractions: Vec<Addr>,
    }

    impl FakeStore {
        fn new(
            declared: &[(&str, &str)],
            prior: &[(&str, &str)],
            present: &[(&str, &str)],
        ) -> Self {
            let items = |v: &[(&str, &str)]| -> Vec<Item> {
                v.iter()
                    .map(|(a, r)| Item::new(Addr::new(*a), Repr::new(*r)))
                    .collect()
            };
            FakeStore {
                declared: items(declared),
                prior: items(prior),
                present: present
                    .iter()
                    .map(|(a, r)| (Addr::new(*a), Repr::new(*r)))
                    .collect(),
                writes: Vec::new(),
                retractions: Vec::new(),
            }
        }
    }

    impl Store for FakeStore {
        fn declared(&self) -> Result<Vec<Item>> {
            Ok(self.declared.clone())
        }

        fn owned(&self) -> Vec<Item> {
            self.prior.clone()
        }

        fn state(&self, addr: &Addr) -> Result<State> {
            let observed = match self.present.get(addr) {
                Some(r) => Observed::Present(r.clone()),
                None => Observed::Absent,
            };
            let recorded = self.prior.iter().find(|i| &i.addr == addr).map(|i| &i.repr);
            let ownership = match (recorded, &observed) {
                (None, _) => Ownership::Unknown,
                (Some(_), Observed::Absent) => Ownership::Removed,
                (Some(p), Observed::Present(o)) if p == o => Ownership::Ours,
                (Some(_), Observed::Present(_)) => Ownership::Diverged,
            };
            Ok(State::new(observed, ownership))
        }

        fn write(&mut self, addr: &Addr) -> Result<()> {
            let repr = self
                .declared
                .iter()
                .find(|i| &i.addr == addr)
                .map(|i| i.repr.clone())
                .expect("write is only called for declared addresses");
            self.present.insert(addr.clone(), repr);
            self.writes.push(addr.clone());
            Ok(())
        }

        fn retract(&mut self, addr: &Addr) -> Result<()> {
            self.present.remove(addr);
            self.retractions.push(addr.clone());
            Ok(())
        }

        fn finish(&mut self, owned: &[Item]) -> Result<Output> {
            Ok(Output {
                document: self
                    .present
                    .iter()
                    .map(|(a, r)| format!("{a}={r}\n"))
                    .collect(),
                prior: Some(owned.to_vec()),
            })
        }
    }

    fn decisions(report: &Report) -> Vec<(String, Decision)> {
        report
            .entries
            .iter()
            .map(|e| (e.addr.to_string(), e.decision))
            .collect()
    }

    #[test]
    fn adds_what_is_absent_and_claims_it() {
        let mut s = FakeStore::new(&[("a", "1")], &[], &[]);
        let (r, out) = run(&mut s, Options::default()).unwrap();
        assert_eq!(decisions(&r), vec![("a".into(), Decision::Add)]);
        assert_eq!(
            out.prior.unwrap(),
            vec![Item::new(Addr::new("a"), Repr::new("1"))]
        );
    }

    #[test]
    fn updates_a_value_this_tool_wrote() {
        // The defect three-way comparison exists to fix: a declaration that
        // changes its mind about a value it put there itself.
        let mut s = FakeStore::new(&[("a", "2")], &[("a", "1")], &[("a", "1")]);
        let (r, _) = run(&mut s, Options::default()).unwrap();
        assert_eq!(decisions(&r), vec![("a".into(), Decision::Update)]);
        assert_eq!(s.present.get(&Addr::new("a")).unwrap(), &Repr::new("2"));
    }

    #[test]
    fn refuses_a_value_it_cannot_claim() {
        let mut s = FakeStore::new(&[("a", "2")], &[], &[("a", "theirs")]);
        let (r, _) = run(&mut s, Options::default()).unwrap();
        assert_eq!(
            decisions(&r),
            vec![("a".into(), Decision::Refuse(Reason::Conflict))]
        );
        assert_eq!(
            s.present.get(&Addr::new("a")).unwrap(),
            &Repr::new("theirs")
        );
        assert!(s.writes.is_empty(), "nothing may be written on a refusal");
    }

    #[test]
    fn retraction_is_off_by_default_but_keeps_the_claim() {
        let mut s = FakeStore::new(&[], &[("a", "1")], &[("a", "1")]);
        let (r, out) = run(&mut s, Options::default()).unwrap();
        assert_eq!(decisions(&r), vec![("a".into(), Decision::Skip)]);
        assert!(s.retractions.is_empty());
        assert_eq!(
            out.prior.unwrap(),
            vec![Item::new(Addr::new("a"), Repr::new("1"))],
            "a suppressed retraction stays claimed so it can be retracted later"
        );
    }

    #[test]
    fn retraction_removes_only_what_was_ours() {
        let mut s = FakeStore::new(&[], &[("ours", "1")], &[("ours", "1"), ("theirs", "x")]);
        let opts = Options {
            retract: true,
            ..Options::default()
        };
        let (r, out) = run(&mut s, opts).unwrap();
        assert_eq!(decisions(&r), vec![("ours".into(), Decision::Retract)]);
        assert!(!s.present.contains_key(&Addr::new("ours")));
        assert!(
            s.present.contains_key(&Addr::new("theirs")),
            "an item this tool never wrote is not walked and not touched"
        );
        assert!(out.prior.unwrap().is_empty());
    }

    #[test]
    fn force_overrides_a_refusal_and_says_so() {
        let mut s = FakeStore::new(&[("a", "2")], &[], &[("a", "theirs")]);
        let opts = Options {
            force: true,
            ..Options::default()
        };
        let (r, _) = run(&mut s, opts).unwrap();
        assert_eq!(decisions(&r), vec![("a".into(), Decision::Update)]);
        assert!(r.entries[0].overridden);
    }

    #[test]
    fn a_second_identical_run_only_skips() {
        let mut s = FakeStore::new(&[("a", "1"), ("b", "2")], &[], &[]);
        let (first, out) = run(&mut s, Options::default()).unwrap();
        assert!(first.changed());

        let prior: Vec<(String, String)> = out
            .prior
            .unwrap()
            .iter()
            .map(|i| (i.addr.to_string(), i.repr.to_string()))
            .collect();
        let prior: Vec<(&str, &str)> = prior
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect();
        let present = prior.clone();

        let mut s2 = FakeStore::new(&[("a", "1"), ("b", "2")], &prior, &present);
        let (second, _) = run(&mut s2, Options::default()).unwrap();
        assert!(!second.changed(), "a settled declaration must not rewrite");
        assert!(!second.has_drift());
        assert_eq!(second.tally().skipped, 2);
    }

    #[test]
    fn adoption_claims_a_matching_value_written_before_provenance() {
        let mut s = FakeStore::new(&[("a", "1")], &[], &[("a", "1")]);
        let (r, out) = run(&mut s, Options::default()).unwrap();
        assert_eq!(decisions(&r), vec![("a".into(), Decision::Skip)]);
        assert_eq!(
            out.prior.unwrap(),
            vec![Item::new(Addr::new("a"), Repr::new("1"))],
            "agreeing on a value adopts it, so a later change can apply"
        );
    }

    #[test]
    fn a_hand_edit_keeps_its_history_across_runs() {
        let mut s = FakeStore::new(&[("a", "1")], &[("a", "1")], &[("a", "edited")]);
        let (r, out) = run(&mut s, Options::default()).unwrap();
        assert_eq!(
            decisions(&r),
            vec![("a".into(), Decision::Refuse(Reason::HandEdited))]
        );
        assert_eq!(
            out.prior.unwrap(),
            vec![Item::new(Addr::new("a"), Repr::new("1"))],
            "keeping the old record is what lets the next run still say hand-edited"
        );
    }

    #[test]
    fn a_deletion_by_someone_else_is_not_undone() {
        let mut s = FakeStore::new(&[("a", "1")], &[("a", "1")], &[]);
        let (r, _) = run(&mut s, Options::default()).unwrap();
        assert_eq!(
            decisions(&r),
            vec![("a".into(), Decision::Refuse(Reason::RemovedByOther))]
        );
        assert!(s.writes.is_empty());
    }

    #[test]
    fn walk_order_is_declaration_then_sorted_remainder() {
        let mut s = FakeStore::new(
            &[("z", "1"), ("a", "2")],
            &[("m", "3"), ("b", "4")],
            &[("m", "3"), ("b", "4")],
        );
        let (r, _) = run(&mut s, Options::default()).unwrap();
        let order: Vec<String> = r.entries.iter().map(|e| e.addr.to_string()).collect();
        assert_eq!(order, vec!["z", "a", "b", "m"]);
    }

    #[test]
    fn tally_counts_each_outcome() {
        let mut s = FakeStore::new(
            &[("add", "1"), ("skip", "2"), ("conflict", "3")],
            &[("skip", "2")],
            &[("skip", "2"), ("conflict", "theirs")],
        );
        let (r, _) = run(&mut s, Options::default()).unwrap();
        let t = r.tally();
        assert_eq!(t.added, 1);
        assert_eq!(t.skipped, 1);
        assert_eq!(t.refused, 1);
        assert!(r.has_drift());
    }
}
