//! Provenance for representations that cannot hold it inline.
//!
//! Deciding whether a value is this tool's to change requires remembering what
//! it wrote. A marker-delimited region can keep that record in the file itself,
//! because text can carry a comment. A JSON document cannot: JSON has no
//! comments, and adding a reserved key would be a change to another program's
//! document that the program never asked for and may reject outright.
//!
//! So the record goes in a file of this tool's own.
//!
//! # Where it lives
//!
//! [`Store::default_path`] resolves `$XDG_STATE_HOME/cfggraft/priors.json`, falling
//! back to `$HOME/.local/state/cfggraft/priors.json`. State, in the XDG sense, is
//! exactly what this is: data a program needs between runs that is neither
//! configuration nor a cache.
//!
//! It is deliberately not kept beside the target, which would litter another
//! program's directory, and not kept in the repository holding the declaration,
//! which would be wrong in a subtler way. The record answers "what did this tool
//! write *here*", and two machines applying the same declaration will hold
//! different answers — legitimately, since their targets differ.
//!
//! # When it is missing
//!
//! A run whose record is unavailable — never created, deleted, or a target read
//! from standard input and so having no path to key on — sees every item as
//! [`Ownership::Unknown`](crate::port::Ownership::Unknown). The decision table
//! then allows only adding what is absent and agreeing with what already
//! matches, so the run degrades to add-only. Capability is lost; data is not.
//! The first run after that re-adopts every item it still agrees with.
//!
//! # Format
//!
//! ```json
//! {
//!   "version": 1,
//!   "targets": {
//!     "/home/me/.config/some-app/settings.json": {
//!       "env.EDITOR": "\"hx\""
//!     }
//!   }
//! }
//! ```
//!
//! Targets are keyed by absolute path, and each holds a map from address to the
//! canonical repr that was written. Both maps are written in sorted order so
//! that the file's own diffs stay readable.
//!
//! An unreadable or malformed record is treated as an empty one rather than as a
//! failure: losing provenance costs capability, and refusing to run costs more.

use crate::atomic;
use crate::error::{Error, Result};
use crate::port::{Addr, Item, Repr};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The version this crate writes. Present so that a future change of shape can
/// be recognised rather than misread.
const VERSION: u64 = 1;

/// The record of what this tool has written, across every target.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    targets: BTreeMap<String, BTreeMap<String, String>>,
}

impl Store {
    /// The default location, `$XDG_STATE_HOME/cfggraft/priors.json`.
    ///
    /// Falls back to `$HOME/.local/state/cfggraft/priors.json`, and to a relative
    /// path if neither variable is set — an environment that exotic has no home
    /// directory to guess at.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from(".local/state"));
        base.join("cfggraft").join("priors.json")
    }

    /// Read the record at `path`, or start an empty one.
    ///
    /// A missing, unreadable, or malformed file yields an empty record. See the
    /// module documentation for why that is a degradation rather than an error.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let targets = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| parse(&v))
            .unwrap_or_default();
        Store { path, targets }
    }

    /// An empty record that writes to `path`.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Store {
            path: path.into(),
            targets: BTreeMap::new(),
        }
    }

    /// Where this record is stored.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What this tool last wrote to `target`.
    ///
    /// Empty for a target never written, which is the add-only case.
    pub fn items(&self, target: &Path) -> Vec<Item> {
        let Some(map) = self.targets.get(&key(target)) else {
            return Vec::new();
        };
        map.iter()
            .map(|(a, r)| Item::new(Addr::new(a.clone()), Repr::new(r.clone())))
            .collect()
    }

    /// Replace what is recorded for `target`.
    ///
    /// An empty `items` removes the target's entry entirely, so a record does
    /// not accumulate rows for targets nothing is claimed in any more.
    pub fn set(&mut self, target: &Path, items: &[Item]) {
        let key = key(target);
        if items.is_empty() {
            self.targets.remove(&key);
            return;
        }
        let map = items
            .iter()
            .map(|i| (i.addr.as_str().to_string(), i.repr.as_str().to_string()))
            .collect();
        self.targets.insert(key, map);
    }

    /// Write the record out, creating its directory if needed.
    pub fn save(&self) -> Result<()> {
        let mut targets = Map::new();
        for (target, items) in &self.targets {
            let mut m = Map::new();
            for (addr, repr) in items {
                m.insert(addr.clone(), Value::String(repr.clone()));
            }
            targets.insert(target.clone(), Value::Object(m));
        }
        let mut root = Map::new();
        root.insert("version".into(), Value::from(VERSION));
        root.insert("targets".into(), Value::Object(targets));

        let mut text = serde_json::to_string_pretty(&Value::Object(root))
            .map_err(|e| Error::parse(&self.path, e))?;
        text.push('\n');
        atomic::write(&self.path, &text)
    }
}

/// Key a target by its absolute path, so that the same file reached by two
/// different relative paths is one entry rather than two.
fn key(target: &Path) -> String {
    std::fs::canonicalize(target)
        .or_else(|_| {
            // The target may not exist yet on a first run, so fall back to
            // resolving it against the working directory without touching disk.
            std::env::current_dir().map(|cwd| cwd.join(target))
        })
        .unwrap_or_else(|_| target.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn parse(v: &Value) -> BTreeMap<String, BTreeMap<String, String>> {
    let Some(targets) = v.get("targets").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    targets
        .iter()
        .filter_map(|(target, items)| {
            let items = items.as_object()?;
            let map = items
                .iter()
                .filter_map(|(a, r)| Some((a.clone(), r.as_str()?.to_string())))
                .collect();
            Some((target.clone(), map))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("cfggraft-prior-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn item(addr: &str, repr: &str) -> Item {
        Item::new(Addr::new(addr), Repr::new(repr))
    }

    #[test]
    fn round_trips_through_the_file() {
        let p = scratch("round-trip.json");
        let target = Path::new("/some/target.json");

        let mut s = Store::empty(&p);
        s.set(target, &[item("env.A", "\"1\""), item("env.B", "\"2\"")]);
        s.save().unwrap();

        let reopened = Store::open(&p);
        let mut got = reopened.items(target);
        got.sort_by(|a, b| a.addr.cmp(&b.addr));
        assert_eq!(got, vec![item("env.A", "\"1\""), item("env.B", "\"2\"")]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn an_unknown_target_has_nothing_recorded() {
        let s = Store::empty(scratch("unused.json"));
        assert!(s.items(Path::new("/never/written.json")).is_empty());
    }

    #[test]
    fn a_missing_file_opens_empty_rather_than_failing() {
        let s = Store::open(scratch("does-not-exist.json"));
        assert!(s.items(Path::new("/x")).is_empty());
    }

    #[test]
    fn malformed_content_degrades_to_empty() {
        let p = scratch("malformed.json");
        fs::write(&p, "{ not json").unwrap();
        let s = Store::open(&p);
        assert!(s.items(Path::new("/x")).is_empty());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn clearing_a_target_drops_its_row() {
        let p = scratch("cleared.json");
        let target = Path::new("/some/target.json");

        let mut s = Store::empty(&p);
        s.set(target, &[item("a", "1")]);
        s.set(target, &[]);
        s.save().unwrap();

        let text = fs::read_to_string(&p).unwrap();
        assert!(!text.contains("/some/target.json"), "got: {text}");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn the_file_is_written_in_sorted_order() {
        let p = scratch("sorted.json");
        let mut s = Store::empty(&p);
        s.set(Path::new("/t.json"), &[item("z", "1"), item("a", "2")]);
        s.save().unwrap();

        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.find("\"a\"").unwrap() < text.find("\"z\"").unwrap(),
            "addresses should be sorted: {text}"
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn default_path_sits_under_the_state_directory() {
        let p = Store::default_path();
        assert!(p.ends_with("cfggraft/priors.json"), "got: {}", p.display());
    }
}
