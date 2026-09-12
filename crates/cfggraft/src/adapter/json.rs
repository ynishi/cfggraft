//! JSON documents, addressed by key path.
//!
//! A declaration is a JSON value, and it expands into one item per leaf. An
//! object contributes an item per key, recursively; an array contributes an item
//! per element; anything else is a single item. So this declaration
//!
//! ```json
//! { "env": { "EDITOR": "hx" }, "permissions": { "deny": ["rm", "dd"] } }
//! ```
//!
//! declares four items, and each is decided on separately. Adding a key to the
//! declaration therefore has no bearing on any other key, and a disagreement
//! about one value never blocks the rest.
//!
//! # Arrays are sets
//!
//! List-valued settings are combined across configuration layers by the programs
//! that read them, so an element is a membership statement rather than a
//! position: declaring `["rm"]` says *`rm` should be denied*, not *the deny list
//! should be exactly this*. Elements are therefore addressed by value and never
//! by index — an index would name a different element as soon as a neighbour
//! were inserted, and provenance recorded under it would point at the wrong
//! value on the next run.
//!
//! The address of an element is its array's path followed by a digest of the
//! element's canonical form, as in `permissions.deny[#3f9a1c4e]`. The digest
//! makes the address unique and stable; the value itself is carried alongside it
//! in provenance and in reports, so nothing depends on reading the digest back.
//!
//! Because an element's value *is* its identity, an element can only be present
//! or absent. It cannot be found changed, which is why a changed element reads
//! as one item withdrawn and another added.
//!
//! # Canonical form
//!
//! Values are compared as JSON text with object keys sorted recursively. The
//! other program may rewrite the document with its keys in a different order,
//! and that is not a change to the value; comparing the text as written would
//! report it as one. Sorting is for comparison only — a value is always written
//! from the declaration, so the declaration's own key order is what lands in the
//! file.
//!
//! # Addressing limits
//!
//! Paths are dotted, so a key containing `.` cannot be addressed, and a key
//! containing `[#` may be misread as an array element when provenance is read
//! back. Neither appears in the configuration formats this addresses.

use crate::error::{Error, Result};
use crate::port::{Addr, Item, Observed, Output, Ownership, Repr, State, Store};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Split a dotted path into segments, tolerating a leading dot.
///
/// Both `env` and `.env` name the same slot; the leading-dot form is what
/// scripts that previously called `jq` will already be passing.
///
/// ```
/// use cfggraft::adapter::json::split_path;
///
/// assert_eq!(split_path(".env.EDITOR"), vec!["env", "EDITOR"]);
/// assert!(split_path("").is_empty());
/// ```
pub fn split_path(path: &str) -> Vec<String> {
    let trimmed = path.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('.').map(str::to_string).collect()
}

/// Render a path for display, naming the document root when empty.
pub fn display_path(segs: &[String]) -> String {
    if segs.is_empty() {
        "<root>".to_string()
    } else {
        segs.join(".")
    }
}

/// Read the value at `segs`, if every segment exists and is reachable.
pub fn get_path<'a>(root: &'a Value, segs: &[String]) -> Option<&'a Value> {
    let mut cur = root;
    for s in segs {
        cur = cur.as_object()?.get(s)?;
    }
    Some(cur)
}

/// The canonical form of a value: JSON text with object keys sorted recursively.
///
/// ```
/// use cfggraft::adapter::json::canonical;
/// use serde_json::json;
///
/// // Key order is not part of the value.
/// assert_eq!(canonical(&json!({"b": 1, "a": 2})).as_str(), r#"{"a":2,"b":1}"#);
/// ```
pub fn canonical(v: &Value) -> Repr {
    Repr::new(canonical_text(v))
}

fn canonical_text(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_unstable();
            let body: Vec<String> = keys
                .iter()
                .map(|k| {
                    let key = Value::String((*k).to_string());
                    format!("{}:{}", key, canonical_text(&m[*k]))
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(a) => {
            let body: Vec<String> = a.iter().map(canonical_text).collect();
            format!("[{}]", body.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

/// How to reach one item inside the document.
#[derive(Debug, Clone)]
enum Locator {
    /// A value at a key path.
    Key(Vec<String>),
    /// An element of the array at a key path, identified by its canonical form.
    Element {
        /// Path of the array holding it.
        path: Vec<String>,
        /// The element's canonical form, which is its identity.
        repr: Repr,
    },
}

/// The address of an array element: the array's path and a digest of the value.
fn element_addr(path: &[String], repr: &Repr) -> Addr {
    let mut h = Sha256::new();
    h.update(repr.as_str().as_bytes());
    let digest = format!("{:x}", h.finalize());
    Addr::new(format!("{}[#{}]", display_path(path), &digest[..8]))
}

/// Recover a locator from an address recorded on a previous run.
fn parse_addr(addr: &Addr, repr: &Repr) -> Locator {
    if let Some(rest) = addr.as_str().strip_suffix(']') {
        if let Some((path, _digest)) = rest.rsplit_once("[#") {
            return Locator::Element {
                path: split_path(path),
                repr: repr.clone(),
            };
        }
    }
    Locator::Key(split_path(addr.as_str()))
}

/// A JSON document being managed at a key path.
#[derive(Debug)]
pub struct JsonDoc {
    root: Value,
    declared: Vec<Item>,
    /// Where each item lives, for declared items and for ones known only to
    /// provenance.
    locators: BTreeMap<Addr, Locator>,
    /// Declared values as written, preserving the declaration's key order.
    values: BTreeMap<Addr, Value>,
    prior: Vec<Item>,
}

impl JsonDoc {
    /// Parse `target` and expand `declaration` into items rooted at `key`.
    ///
    /// An empty target is an empty document, so a first run can create the file.
    /// `prior` is what a previous run recorded for this target; an empty slice
    /// leaves every item unowned and reduces the run to add-only.
    pub fn new(
        target: &str,
        target_name: &std::path::Path,
        declaration: &Value,
        key: &[String],
        prior: Vec<Item>,
    ) -> Result<Self> {
        let root: Value = if target.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(target).map_err(|e| Error::parse(target_name, e))?
        };

        let mut declared = Vec::new();
        let mut locators = BTreeMap::new();
        let mut values = BTreeMap::new();
        expand(key, declaration, &mut |addr, locator, repr, value| {
            declared.push(Item::new(addr.clone(), repr));
            locators.insert(addr.clone(), locator);
            values.insert(addr, value);
        });

        for item in &prior {
            locators
                .entry(item.addr.clone())
                .or_insert_with(|| parse_addr(&item.addr, &item.repr));
        }

        Ok(JsonDoc {
            root,
            declared,
            locators,
            values,
            prior,
        })
    }

    /// The document as it now stands, for callers that render it themselves.
    pub fn value(&self) -> &Value {
        &self.root
    }

    fn locator(&self, addr: &Addr) -> Result<&Locator> {
        self.locators
            .get(addr)
            .ok_or_else(|| Error::invalid(format!("no such item: {addr}")))
    }

    fn prior_repr(&self, addr: &Addr) -> Option<&Repr> {
        self.prior.iter().find(|i| &i.addr == addr).map(|i| &i.repr)
    }

    /// Walk to the container holding `segs`, reporting where the path stops.
    fn resolve<'a>(root: &'a Value, segs: &[String]) -> Resolved<'a> {
        let mut cur = root;
        for (i, s) in segs.iter().enumerate() {
            let Some(obj) = cur.as_object() else {
                return Resolved::Blocked {
                    at: display_path(&segs[..i]),
                    found: cur,
                };
            };
            match obj.get(s) {
                Some(next) => cur = next,
                None => return Resolved::Absent,
            }
        }
        Resolved::Found(cur)
    }
}

/// Where a walk down a key path ended.
enum Resolved<'a> {
    /// The path exists and holds this value.
    Found(&'a Value),
    /// A segment along the way is missing.
    Absent,
    /// A value along the way is not an object, so the path cannot continue.
    Blocked {
        /// The path of the offending value.
        at: String,
        /// What was found there instead of an object.
        found: &'a Value,
    },
}

/// A value that blocks a path is reported as a disagreement about that value,
/// not as a failure: the target is telling us it disagrees about the shape, and
/// that is drift like any other.
fn blocked_state(at: &str, found: &Value) -> State {
    State::new(Observed::Present(canonical(found)), Ownership::Unknown).with_note(format!(
        "`{at}` is {}, so the path cannot be followed",
        type_name(found)
    ))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Expand a declared value into leaf items.
///
/// An empty object or array is a leaf: declaring `{}` asks for an empty object
/// to exist, and expanding it into nothing would silently declare nothing at
/// all.
fn expand(path: &[String], value: &Value, emit: &mut impl FnMut(Addr, Locator, Repr, Value)) {
    match value {
        Value::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let mut child = path.to_vec();
                child.push(k.clone());
                expand(&child, v, emit);
            }
        }
        Value::Array(elems) if !elems.is_empty() => {
            for e in elems {
                let repr = canonical(e);
                let addr = element_addr(path, &repr);
                let locator = Locator::Element {
                    path: path.to_vec(),
                    repr: repr.clone(),
                };
                emit(addr, locator, repr, e.clone());
            }
        }
        leaf => {
            let addr = Addr::new(display_path(path));
            emit(
                addr,
                Locator::Key(path.to_vec()),
                canonical(leaf),
                leaf.clone(),
            );
        }
    }
}

/// Borrow the container at `segs`, creating missing objects along the way.
///
/// A non-object encountered on the way down is an error rather than something to
/// replace: replacing it would discard the other program's data, which is the
/// one thing this crate never does. Reaching this is not expected, because such
/// a path reads as [`Resolved::Blocked`] and is refused before any write.
fn ensure_path<'a>(root: &'a mut Value, segs: &[String]) -> Result<&'a mut Value> {
    if root.is_null() {
        *root = Value::Object(Map::new());
    }
    let mut cur = root;
    let mut walked = Vec::new();
    for s in segs {
        walked.push(s.clone());
        if !cur.is_object() {
            return Err(Error::invalid(format!(
                "cannot descend into `{}`: {} is not an object",
                display_path(&walked),
                display_path(&walked[..walked.len() - 1])
            )));
        }
        cur = cur
            .as_object_mut()
            .expect("checked immediately above")
            .entry(s.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    Ok(cur)
}

impl Store for JsonDoc {
    fn declared(&self) -> Result<Vec<Item>> {
        Ok(self.declared.clone())
    }

    fn owned(&self) -> Vec<Item> {
        self.prior.clone()
    }

    fn state(&self, addr: &Addr) -> Result<State> {
        let observed = match self.locator(addr)? {
            Locator::Key(path) => match JsonDoc::resolve(&self.root, path) {
                Resolved::Found(v) => Observed::Present(canonical(v)),
                Resolved::Absent => Observed::Absent,
                Resolved::Blocked { at, found } => return Ok(blocked_state(&at, found)),
            },
            Locator::Element { path, repr } => match JsonDoc::resolve(&self.root, path) {
                Resolved::Found(Value::Array(elems)) => {
                    if elems.iter().any(|e| &canonical(e) == repr) {
                        Observed::Present(repr.clone())
                    } else {
                        Observed::Absent
                    }
                }
                Resolved::Found(other) => {
                    return Ok(blocked_state(&display_path(path), other));
                }
                Resolved::Absent => Observed::Absent,
                Resolved::Blocked { at, found } => return Ok(blocked_state(&at, found)),
            },
        };

        let ownership = match (self.prior_repr(addr), &observed) {
            (None, _) => Ownership::Unknown,
            (Some(_), Observed::Absent) => Ownership::Removed,
            (Some(p), Observed::Present(o)) if p == o => Ownership::Ours,
            (Some(p), Observed::Present(_)) => {
                return Ok(State::new(observed, Ownership::Diverged)
                    .with_note(format!("this tool last wrote {}", p.truncated(48))));
            }
        };
        Ok(State::new(observed, ownership))
    }

    fn write(&mut self, addr: &Addr) -> Result<()> {
        let locator = self.locator(addr)?.clone();
        let value = self
            .values
            .get(addr)
            .ok_or_else(|| Error::invalid(format!("{addr} is not declared")))?
            .clone();

        match locator {
            Locator::Key(path) => {
                let Some((last, parents)) = path.split_last() else {
                    // The declaration named the document root, so it replaces
                    // the whole document rather than a key within it.
                    self.root = value;
                    return Ok(());
                };
                let parent = ensure_path(&mut self.root, parents)?;
                let map = parent.as_object_mut().ok_or_else(|| {
                    Error::invalid(format!(
                        "cannot write `{addr}`: `{}` is not an object",
                        display_path(parents)
                    ))
                })?;
                map.insert(last.clone(), value);
            }
            Locator::Element { path, repr } => {
                let slot = ensure_array(&mut self.root, &path)?;
                if !slot.iter().any(|e| canonical(e) == repr) {
                    slot.push(value);
                }
            }
        }
        Ok(())
    }

    fn retract(&mut self, addr: &Addr) -> Result<()> {
        let locator = self.locator(addr)?.clone();
        match locator {
            Locator::Key(path) => {
                let Some((last, parents)) = path.split_last() else {
                    self.root = Value::Object(Map::new());
                    return Ok(());
                };
                if let Resolved::Found(_) = JsonDoc::resolve(&self.root, parents) {
                    if let Some(map) =
                        value_at_mut(&mut self.root, parents).and_then(Value::as_object_mut)
                    {
                        map.remove(last);
                    }
                }
            }
            Locator::Element { path, repr } => {
                if let Some(arr) = value_at_mut(&mut self.root, &path).and_then(Value::as_array_mut)
                {
                    arr.retain(|e| canonical(e) != repr);
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, owned: &[Item]) -> Result<Output> {
        // Two-space indent with a trailing newline, matching how the programs
        // that own these files write them. Matching the existing style keeps a
        // diff to the items actually touched.
        let mut document = serde_json::to_string_pretty(&self.root)
            .map_err(|e| Error::invalid(format!("cannot render the document: {e}")))?;
        document.push('\n');
        Ok(Output {
            document,
            prior: Some(owned.to_vec()),
        })
    }
}

/// Borrow the value at `segs`, if it is reachable.
fn value_at_mut<'a>(root: &'a mut Value, segs: &[String]) -> Option<&'a mut Value> {
    let mut cur = root;
    for s in segs {
        cur = cur.as_object_mut()?.get_mut(s)?;
    }
    Some(cur)
}

/// Borrow the array at `segs`, creating it and its parents if absent.
fn ensure_array<'a>(root: &'a mut Value, segs: &[String]) -> Result<&'a mut Vec<Value>> {
    let Some((last, parents)) = segs.split_last() else {
        return Err(Error::invalid(
            "the document root cannot be an array element",
        ));
    };
    let parent = ensure_path(root, parents)?;
    let map = parent.as_object_mut().ok_or_else(|| {
        Error::invalid(format!(
            "cannot write into `{}`: it is not an object",
            display_path(parents)
        ))
    })?;
    let slot = map
        .entry(last.clone())
        .or_insert_with(|| Value::Array(Vec::new()));
    slot.as_array_mut().ok_or_else(|| {
        Error::invalid(format!(
            "cannot add to `{}`: it is not an array",
            display_path(segs)
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{run, Options};
    use crate::policy::{Decision, Reason};
    use serde_json::json;
    use std::path::Path;

    /// Build a document the way the command line does: the declaration is the
    /// value *at* the key path within the fragment, not the whole fragment.
    fn doc(target: &str, fragment: Value, key: &str, prior: &[(&str, &str)]) -> JsonDoc {
        let prior = prior
            .iter()
            .map(|(a, r)| Item::new(Addr::new(*a), Repr::new(*r)))
            .collect();
        let segs = split_path(key);
        let declaration = get_path(&fragment, &segs)
            .cloned()
            .expect("the fragment declares something at the key path");
        JsonDoc::new(target, Path::new("target.json"), &declaration, &segs, prior)
            .expect("target parses")
    }

    /// The rendered document, what is now owned, and what was decided.
    type Applied = (String, Vec<(String, String)>, Vec<(String, Decision)>);

    /// Apply a declaration and hand back the rendered document and what is now
    /// owned, so a second run can be written the way a real one happens.
    fn apply(
        target: &str,
        declaration: Value,
        key: &str,
        prior: &[(&str, &str)],
        opts: Options,
    ) -> Applied {
        let mut d = doc(target, declaration, key, prior);
        let (report, out) = run(&mut d, opts).expect("run");
        let owned = out
            .prior
            .unwrap()
            .iter()
            .map(|i| (i.addr.to_string(), i.repr.to_string()))
            .collect();
        let decisions = report
            .entries
            .iter()
            .map(|e| (e.addr.to_string(), e.decision))
            .collect();
        (out.document, owned, decisions)
    }

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).expect("rendered document parses")
    }

    #[test]
    fn adds_an_absent_key() {
        let (text, _, d) = apply(
            r#"{"env":{"A":"1"}}"#,
            json!({"env": {"B": "2"}}),
            ".env",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text)["env"]["B"], json!("2"));
        assert_eq!(parsed(&text)["env"]["A"], json!("1"), "untouched");
        assert_eq!(d, vec![("env.B".into(), Decision::Add)]);
    }

    #[test]
    fn skips_an_equal_key_and_adopts_it() {
        let (_, owned, d) = apply(
            r#"{"env":{"A":"1"}}"#,
            json!({"env": {"A": "1"}}),
            ".env",
            &[],
            Options::default(),
        );
        assert_eq!(d, vec![("env.A".into(), Decision::Skip)]);
        assert_eq!(owned, vec![("env.A".into(), "\"1\"".into())]);
    }

    #[test]
    fn refuses_a_value_it_never_wrote() {
        let (text, _, d) = apply(
            r#"{"env":{"A":"1"}}"#,
            json!({"env": {"A": "2"}}),
            ".env",
            &[],
            Options::default(),
        );
        assert_eq!(
            parsed(&text)["env"]["A"],
            json!("1"),
            "target value survives"
        );
        assert_eq!(
            d,
            vec![("env.A".into(), Decision::Refuse(Reason::Conflict))]
        );
    }

    #[test]
    fn updates_a_value_it_did_write() {
        // Ownership is what separates this from the test above: same target,
        // same declaration, different provenance.
        let (text, _, d) = apply(
            r#"{"env":{"A":"1"}}"#,
            json!({"env": {"A": "2"}}),
            ".env",
            &[("env.A", "\"1\"")],
            Options::default(),
        );
        assert_eq!(parsed(&text)["env"]["A"], json!("2"));
        assert_eq!(d, vec![("env.A".into(), Decision::Update)]);
    }

    #[test]
    fn a_changed_declaration_settles_in_one_run() {
        let first = apply(
            "{}",
            json!({"env": {"A": "1"}}),
            ".env",
            &[],
            Options::default(),
        );
        let prior: Vec<(&str, &str)> = first
            .1
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect();

        let (text, _, d) = apply(
            &first.0,
            json!({"env": {"A": "2"}}),
            ".env",
            &prior,
            Options::default(),
        );
        assert_eq!(parsed(&text)["env"]["A"], json!("2"));
        assert_eq!(d, vec![("env.A".into(), Decision::Update)]);
    }

    #[test]
    fn array_elements_are_added_without_duplicating() {
        let (text, _, _) = apply(
            r#"{"permissions":{"deny":["a","b"]}}"#,
            json!({"permissions": {"deny": ["b", "c"]}}),
            ".permissions.deny",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text)["permissions"]["deny"], json!(["a", "b", "c"]));
    }

    #[test]
    fn array_union_is_idempotent() {
        let target = r#"{"permissions":{"deny":["a","b"]}}"#;
        let decl = json!({"permissions": {"deny": ["b", "c"]}});
        let (once, owned, _) = apply(
            target,
            decl.clone(),
            ".permissions.deny",
            &[],
            Options::default(),
        );
        let prior: Vec<(&str, &str)> = owned
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect();
        let (twice, _, d) = apply(&once, decl, ".permissions.deny", &prior, Options::default());
        assert_eq!(parsed(&once), parsed(&twice));
        assert!(d.iter().all(|(_, d)| *d == Decision::Skip));
    }

    #[test]
    fn a_withdrawn_element_is_removed_and_a_foreign_one_is_not() {
        // The whole point of addressing elements by value: an element this tool
        // put there can be taken back, and one it did not put there cannot be
        // reached at all.
        let first = apply(
            "{}",
            json!({"permissions": {"deny": ["old"]}}),
            ".permissions.deny",
            &[],
            Options::default(),
        );
        let target = first
            .0
            .replace(r#""old""#, "\"old\",\n      \"added-by-the-other-program\"");
        let prior: Vec<(&str, &str)> = first
            .1
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect();

        let opts = Options {
            retract: true,
            ..Options::default()
        };
        let (text, _, d) = apply(
            &target,
            json!({"permissions": {"deny": ["new"]}}),
            ".permissions.deny",
            &prior,
            opts,
        );
        assert_eq!(
            parsed(&text)["permissions"]["deny"],
            json!(["added-by-the-other-program", "new"])
        );
        assert!(d.iter().any(|(_, d)| *d == Decision::Retract));
        assert!(d.iter().any(|(_, d)| *d == Decision::Add));
    }

    #[test]
    fn without_retraction_a_withdrawn_element_stays() {
        let first = apply(
            "{}",
            json!({"permissions": {"deny": ["old"]}}),
            ".permissions.deny",
            &[],
            Options::default(),
        );
        let prior: Vec<(&str, &str)> = first
            .1
            .iter()
            .map(|(a, r)| (a.as_str(), r.as_str()))
            .collect();
        let (text, owned, _) = apply(
            &first.0,
            json!({"permissions": {"deny": ["new"]}}),
            ".permissions.deny",
            &prior,
            Options::default(),
        );
        assert_eq!(parsed(&text)["permissions"]["deny"], json!(["old", "new"]));
        assert_eq!(owned.len(), 2, "the old element stays claimed");
    }

    #[test]
    fn creates_a_missing_parent_path() {
        let (text, _, _) = apply(
            "{}",
            json!({"permissions": {"deny": ["x"]}}),
            ".permissions.deny",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text)["permissions"]["deny"], json!(["x"]));
    }

    #[test]
    fn creates_a_missing_parent_for_a_scalar() {
        let (text, _, d) = apply(
            "{}",
            json!({"a": {"b": 7}}),
            ".a.b",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text)["a"]["b"], json!(7));
        assert_eq!(d, vec![("a.b".into(), Decision::Add)]);
    }

    #[test]
    fn recurses_into_nested_objects_as_separate_items() {
        let (text, _, d) = apply(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash"}]}}"#,
            json!({"hooks": {"Stop": [{"matcher": "x"}], "PreToolUse": [{"matcher": "Read"}]}}),
            ".hooks",
            &[],
            Options::default(),
        );
        assert_eq!(
            parsed(&text)["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(parsed(&text)["hooks"].get("Stop").is_some());
        assert_eq!(d.len(), 2, "one item per element, decided separately");
    }

    #[test]
    fn key_order_within_an_element_is_not_a_change() {
        // The other program may rewrite the document with its keys reordered.
        let (text, _, d) = apply(
            r#"{"hooks":{"Stop":[{"b":2,"a":1}]}}"#,
            json!({"hooks": {"Stop": [{"a": 1, "b": 2}]}}),
            ".hooks",
            &[],
            Options::default(),
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].1, Decision::Skip);
        assert_eq!(parsed(&text)["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_type_mismatch_is_drift_not_a_replacement() {
        let (text, _, d) = apply(
            r#"{"env":{"A":{"nested":true}}}"#,
            json!({"env": {"A": "scalar"}}),
            ".env",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text)["env"]["A"], json!({"nested": true}));
        assert_eq!(
            d,
            vec![("env.A".into(), Decision::Refuse(Reason::Conflict))]
        );
    }

    #[test]
    fn a_blocking_parent_is_reported_rather_than_replaced() {
        let mut d = doc(
            r#"{"permissions":"not-an-object"}"#,
            json!({"permissions": {"deny": ["x"]}}),
            ".permissions.deny",
            &[],
        );
        let (report, out) = run(&mut d, Options::default()).unwrap();
        assert!(report.has_drift());
        assert_eq!(
            parsed(&out.document)["permissions"],
            json!("not-an-object"),
            "the other program's value survives"
        );
        assert!(
            report.entries[0].note.as_ref().unwrap().contains("string"),
            "the report should say what blocked the path"
        );
    }

    #[test]
    fn declaring_at_the_root_is_supported() {
        let (text, _, d) = apply(
            r#"{"keep":1}"#,
            json!({"add": 2}),
            "",
            &[],
            Options::default(),
        );
        assert_eq!(parsed(&text), json!({"keep": 1, "add": 2}));
        assert_eq!(d, vec![("add".into(), Decision::Add)]);
    }

    #[test]
    fn an_empty_container_is_declared_as_a_value() {
        let (text, _, d) = apply("{}", json!({"a": {}}), ".a", &[], Options::default());
        assert_eq!(parsed(&text)["a"], json!({}));
        assert_eq!(d, vec![("a".into(), Decision::Add)]);
    }

    #[test]
    fn an_empty_target_is_an_empty_document() {
        let (text, _, _) = apply("", json!({"a": 1}), ".a", &[], Options::default());
        assert_eq!(parsed(&text), json!({"a": 1}));
    }

    #[test]
    fn the_rendered_document_keeps_the_usual_shape() {
        let (text, _, _) = apply("{}", json!({"a": {"b": 1}}), ".a", &[], Options::default());
        assert!(
            text.starts_with("{\n  \"a\": {\n    \"b\": 1"),
            "got: {text}"
        );
        assert!(text.ends_with("}\n"));
    }

    #[test]
    fn canonical_form_sorts_nested_keys_only_for_comparison() {
        assert_eq!(
            canonical(&json!({"b": {"d": 1, "c": 2}, "a": 3})).as_str(),
            r#"{"a":3,"b":{"c":2,"d":1}}"#
        );
    }

    #[test]
    fn element_addresses_are_stable_and_distinct() {
        let path = vec!["permissions".to_string(), "deny".to_string()];
        let a = element_addr(&path, &canonical(&json!("x")));
        let b = element_addr(&path, &canonical(&json!("y")));
        assert_eq!(a, element_addr(&path, &canonical(&json!("x"))));
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("permissions.deny[#"));
    }

    #[test]
    fn an_element_address_round_trips_from_provenance() {
        let path = vec!["permissions".to_string(), "deny".to_string()];
        let repr = canonical(&json!("x"));
        let addr = element_addr(&path, &repr);
        match parse_addr(&addr, &repr) {
            Locator::Element { path: p, repr: r } => {
                assert_eq!(p, path);
                assert_eq!(r, repr);
            }
            other => panic!("expected an element locator, got {other:?}"),
        }
        match parse_addr(&Addr::new("env.A"), &repr) {
            Locator::Key(p) => assert_eq!(p, vec!["env".to_string(), "A".to_string()]),
            other => panic!("expected a key locator, got {other:?}"),
        }
    }

    #[test]
    fn descending_through_a_scalar_when_writing_is_an_error() {
        let mut root = json!({"permissions": "not-an-object"});
        let err = ensure_path(&mut root, &split_path(".permissions.deny")).unwrap_err();
        assert!(err.to_string().contains("permissions.deny"), "got: {err}");
    }
}
