//! The representations this crate can manage.
//!
//! Each adapter answers the same four questions about a different kind of
//! target: how an item is addressed, what is at an address, whether that value
//! is this tool's, and how to write or withdraw it. Nothing about *which* change
//! to make lives here — [`crate::policy`] decides that once, for all of them.
//!
//! | adapter | target | address | provenance |
//! |---|---|---|---|
//! | [`json::JsonDoc`] | a JSON document | dotted key path | side-car, via [`crate::prior`] |
//! | [`marker::MarkerSpan`] | a marker-delimited span of a text file | marker name | inline, as a digest |
//!
//! # Dispatch
//!
//! [`Adapter`] is an enum rather than a trait object because the set of
//! representations is closed within this crate: an enum keeps the dispatch
//! visible and costs nothing at runtime, and [`Store`] exists to state the
//! contract an implementation must meet rather than to be implemented from
//! outside.
//!
//! Adding a representation is a new module, a new variant, and two lines in each
//! match below.

pub mod json;
pub mod marker;

use crate::error::Result;
use crate::port::{Addr, Item, Output, State, Store};

pub use json::JsonDoc;
pub use marker::{CommentStyle, MarkerSpan};

/// One of the representations this crate manages.
pub enum Adapter {
    /// A JSON document addressed by key path.
    Json(JsonDoc),
    /// A marker-delimited span of a text file.
    Marker(MarkerSpan),
}

impl Store for Adapter {
    fn declared(&self) -> Result<Vec<Item>> {
        match self {
            Adapter::Json(a) => a.declared(),
            Adapter::Marker(a) => a.declared(),
        }
    }

    fn owned(&self) -> Vec<Item> {
        match self {
            Adapter::Json(a) => a.owned(),
            Adapter::Marker(a) => a.owned(),
        }
    }

    fn state(&self, addr: &Addr) -> Result<State> {
        match self {
            Adapter::Json(a) => a.state(addr),
            Adapter::Marker(a) => a.state(addr),
        }
    }

    fn write(&mut self, addr: &Addr) -> Result<()> {
        match self {
            Adapter::Json(a) => a.write(addr),
            Adapter::Marker(a) => a.write(addr),
        }
    }

    fn retract(&mut self, addr: &Addr) -> Result<()> {
        match self {
            Adapter::Json(a) => a.retract(addr),
            Adapter::Marker(a) => a.retract(addr),
        }
    }

    fn finish(&mut self, owned: &[Item]) -> Result<Output> {
        match self {
            Adapter::Json(a) => a.finish(owned),
            Adapter::Marker(a) => a.finish(owned),
        }
    }
}
