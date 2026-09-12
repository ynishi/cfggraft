//! The decision, for every representation.
//!
//! [`decide`] is the only place in this crate where an outcome is chosen. It
//! knows nothing about JSON, markers, or files: it reads a declaration, an
//! observation, and an ownership verdict, and returns what to do.
//!
//! Keeping it in one total function is what makes a new representation cheap.
//! An adapter that answers "what is here, and is it ours?" inherits every rule
//! below without restating any of it — and cannot accidentally contradict
//! another adapter, because there is no second copy to drift from.

use crate::port::{Observed, Ownership, Repr, State};

/// Why a change was refused.
///
/// Each variant names a different writer and a different remedy, so they are not
/// collapsed into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A value is present that this tool cannot show it wrote. It may belong to
    /// the other program, or predate any provenance. Either way the declaration
    /// cannot claim it.
    Conflict,
    /// A value this tool wrote has since been changed by someone else.
    HandEdited,
    /// An item this tool wrote has since been deleted by someone else.
    RemovedByOther,
}

impl Reason {
    /// A short label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Reason::Conflict => "conflict",
            Reason::HandEdited => "hand-edited",
            Reason::RemovedByOther => "removed",
        }
    }
}

/// What to do about one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is there; write the declared value.
    Add,
    /// The target already agrees; leave it alone.
    Skip,
    /// A value this tool wrote is there and the declaration has moved on;
    /// replace it.
    Update,
    /// A value this tool wrote is there and is no longer declared; remove it.
    Retract,
    /// The value is not this tool's to change. Nothing is written.
    Refuse(Reason),
}

impl Decision {
    /// Whether this decision alters the target.
    pub fn changes_target(self) -> bool {
        matches!(self, Decision::Add | Decision::Update | Decision::Retract)
    }

    /// Whether this decision reports drift — a difference this tool declined to
    /// overwrite.
    pub fn is_drift(self) -> bool {
        matches!(self, Decision::Refuse(_))
    }

    /// A short label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Decision::Add => "add",
            Decision::Skip => "skip",
            Decision::Update => "update",
            Decision::Retract => "retract",
            Decision::Refuse(r) => r.label(),
        }
    }
}

/// Decide what to do about one item.
///
/// `declared` is the value the declaration asks for, or `None` for an item that
/// provenance records but the declaration no longer mentions. `state` is what
/// the target holds and whose it is.
///
/// # The table
///
/// | declared | ownership | observed | result |
/// |---|---|---|---|
/// | yes | [`Unknown`](Ownership::Unknown) | absent | [`Add`](Decision::Add) |
/// | yes | [`Unknown`](Ownership::Unknown) | equal | [`Skip`](Decision::Skip) |
/// | yes | [`Unknown`](Ownership::Unknown) | differs | [`Refuse`](Reason::Conflict) |
/// | yes | [`Ours`](Ownership::Ours) | equal | [`Skip`](Decision::Skip) |
/// | yes | [`Ours`](Ownership::Ours) | differs | [`Update`](Decision::Update) |
/// | yes | [`Diverged`](Ownership::Diverged) | any | [`Refuse`](Reason::HandEdited) |
/// | yes | [`Removed`](Ownership::Removed) | any | [`Refuse`](Reason::RemovedByOther) |
/// | no | [`Ours`](Ownership::Ours) | present | [`Retract`](Decision::Retract) |
/// | no | anything else | any | [`Skip`](Decision::Skip) |
///
/// # Why ownership decides and value equality does not
///
/// The middle two rows are the reason this function takes three inputs. Both
/// observe a value that differs from the declaration; they differ only in
/// whether this tool can show it wrote that value. Without that distinction the
/// only safe answer to "the value differs" is to refuse, which makes a
/// declaration permanently unable to change its own mind — and, for a set, makes
/// a withdrawn element impossible to clean up, so every edit to the declaration
/// leaves its predecessor behind for good.
///
/// # Why a deletion is honoured
///
/// [`Removed`](Ownership::Removed) refuses rather than re-adding. Provenance
/// proves the item was ours, so re-adding would be safe for the *data* — but the
/// other writer's deletion is an edit like any other, and reinstating it on
/// every run produces a value that cannot be got rid of. The remedy is to stop
/// declaring it, which the report says plainly.
///
/// # Examples
///
/// ```
/// use cfggraft::policy::{decide, Decision, Reason};
/// use cfggraft::port::{Observed, Ownership, Repr, State};
///
/// let declared = Repr::new("\"dark\"");
///
/// // Nothing there yet.
/// let s = State::new(Observed::Absent, Ownership::Unknown);
/// assert_eq!(decide(Some(&declared), &s), Decision::Add);
///
/// // Someone else's value: kept, and reported.
/// let s = State::new(Observed::Present(Repr::new("\"light\"")), Ownership::Unknown);
/// assert_eq!(decide(Some(&declared), &s), Decision::Refuse(Reason::Conflict));
///
/// // The same value, but ours: updated.
/// let s = State::new(Observed::Present(Repr::new("\"light\"")), Ownership::Ours);
/// assert_eq!(decide(Some(&declared), &s), Decision::Update);
///
/// // Ours, and no longer declared: withdrawn.
/// let s = State::new(Observed::Present(Repr::new("\"light\"")), Ownership::Ours);
/// assert_eq!(decide(None, &s), Decision::Retract);
/// ```
pub fn decide(declared: Option<&Repr>, state: &State) -> Decision {
    let Some(declared) = declared else {
        // Not declared. Only something provably ours may be withdrawn.
        return match (state.ownership, &state.observed) {
            (Ownership::Ours, Observed::Present(_)) => Decision::Retract,
            _ => Decision::Skip,
        };
    };

    match state.ownership {
        Ownership::Diverged => Decision::Refuse(Reason::HandEdited),
        Ownership::Removed => Decision::Refuse(Reason::RemovedByOther),
        Ownership::Ours => {
            if state.observed.equals(declared) {
                Decision::Skip
            } else {
                // Provably ours, so the old value is this tool's to replace.
                Decision::Update
            }
        }
        Ownership::Unknown => match &state.observed {
            Observed::Absent => Decision::Add,
            Observed::Present(o) if o == declared => {
                // Already agreed. The caller records ownership, adopting the
                // item so that later changes to the declaration can apply.
                Decision::Skip
            }
            Observed::Present(_) => Decision::Refuse(Reason::Conflict),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repr(s: &str) -> Repr {
        Repr::new(s)
    }

    fn state(observed: Observed, ownership: Ownership) -> State {
        State::new(observed, ownership)
    }

    fn present(s: &str) -> Observed {
        Observed::Present(repr(s))
    }

    #[test]
    fn unknown_absent_is_added() {
        let s = state(Observed::Absent, Ownership::Unknown);
        assert_eq!(decide(Some(&repr("1")), &s), Decision::Add);
    }

    #[test]
    fn unknown_matching_is_adopted_by_skipping() {
        let s = state(present("1"), Ownership::Unknown);
        assert_eq!(decide(Some(&repr("1")), &s), Decision::Skip);
    }

    #[test]
    fn unknown_differing_is_refused_and_not_updated() {
        let s = state(present("1"), Ownership::Unknown);
        assert_eq!(
            decide(Some(&repr("2")), &s),
            Decision::Refuse(Reason::Conflict)
        );
    }

    #[test]
    fn ours_differing_is_updated() {
        let s = state(present("1"), Ownership::Ours);
        assert_eq!(decide(Some(&repr("2")), &s), Decision::Update);
    }

    #[test]
    fn ours_matching_is_skipped() {
        let s = state(present("2"), Ownership::Ours);
        assert_eq!(decide(Some(&repr("2")), &s), Decision::Skip);
    }

    #[test]
    fn diverged_is_refused_whatever_is_declared() {
        let s = state(present("edited"), Ownership::Diverged);
        assert_eq!(
            decide(Some(&repr("x")), &s),
            Decision::Refuse(Reason::HandEdited)
        );
    }

    #[test]
    fn removed_is_not_resurrected() {
        let s = state(Observed::Absent, Ownership::Removed);
        assert_eq!(
            decide(Some(&repr("x")), &s),
            Decision::Refuse(Reason::RemovedByOther)
        );
    }

    #[test]
    fn undeclared_and_ours_is_retracted() {
        let s = state(present("old"), Ownership::Ours);
        assert_eq!(decide(None, &s), Decision::Retract);
    }

    #[test]
    fn undeclared_and_not_ours_is_left_alone() {
        for own in [Ownership::Unknown, Ownership::Diverged, Ownership::Removed] {
            let s = state(present("theirs"), own);
            assert_eq!(decide(None, &s), Decision::Skip, "ownership {own:?}");
        }
    }

    #[test]
    fn nothing_is_written_unless_ours_or_absent() {
        // The safety property the whole crate rests on: a decision that writes
        // over an existing value occurs only when that value is ours.
        for own in [
            Ownership::Unknown,
            Ownership::Ours,
            Ownership::Diverged,
            Ownership::Removed,
        ] {
            let s = state(present("theirs"), own);
            let d = decide(Some(&repr("mine")), &s);
            if d.changes_target() {
                assert_eq!(own, Ownership::Ours, "would overwrite with {own:?}");
            }
        }
    }

    #[test]
    fn an_agreed_declaration_reapplies_as_skip() {
        // Idempotence: after any applying decision the item is ours and equal,
        // which is the Skip row.
        let s = state(present("v"), Ownership::Ours);
        assert_eq!(decide(Some(&repr("v")), &s), Decision::Skip);
    }

    #[test]
    fn labels_are_distinct_per_outcome() {
        let labels = [
            Decision::Add.label(),
            Decision::Skip.label(),
            Decision::Update.label(),
            Decision::Retract.label(),
            Decision::Refuse(Reason::Conflict).label(),
            Decision::Refuse(Reason::HandEdited).label(),
            Decision::Refuse(Reason::RemovedByOther).label(),
        ];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels collide: {labels:?}");
    }

    #[test]
    fn only_refusal_is_drift() {
        assert!(Decision::Refuse(Reason::Conflict).is_drift());
        assert!(!Decision::Add.is_drift());
        assert!(!Decision::Skip.is_drift());
        assert!(!Decision::Update.is_drift());
        assert!(!Decision::Retract.is_drift());
    }
}
