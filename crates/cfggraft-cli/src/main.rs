//! Command line front end.
//!
//! This binary parses arguments, reads and writes files, prints the report, and
//! maps the outcome to an exit status. Every decision about what to change is
//! made by the library — see the crate documentation of `cfggraft` for the model and
//! the decision table.
//!
//! # Exit status
//!
//! | | |
//! |---|---|
//! | 0 | applied, or already up to date |
//! | 1 | failure — bad input, an unreadable file, a missing marker |
//! | 2 | drift — a difference this tool declined to overwrite. Nothing was written. |
//!
//! 2 is reserved for drift alone. A check that cannot tell *the generator
//! crashed* from *there is no difference* is a green light that checks nothing.
//!
//! # When ownership is recorded
//!
//! Provenance is written only when the target itself is written: with
//! `--in-place`, and without `--check`. Rendering to standard output leaves the
//! target untouched, so claiming to have written it would be false, and the next
//! run would act on a record of something that never happened.
//!
//! A consequence worth knowing: a pipeline that never uses `--in-place` never
//! accumulates provenance, so it stays add-only.

use cfggraft::adapter::json::{self, JsonDoc};
use cfggraft::adapter::marker::{CommentStyle as MarkerStyle, MarkerSpan};
use cfggraft::adapter::Adapter;
use cfggraft::engine::{self, Entry, Options, Report};
use cfggraft::policy::Decision;
use cfggraft::{atomic, prior, Error, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const EXIT_OK: u8 = 0;
const EXIT_FAIL: u8 = 1;
const EXIT_DRIFT: u8 = 2;

#[derive(Parser)]
#[command(
    name = "cfggraft",
    version,
    about = "Inject declared items into config files owned by another program.",
    after_help = "Exit codes: 0 applied or up to date, 1 failure, 2 drift (nothing overwritten)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage items of a JSON document, addressed by key path.
    Merge(MergeArgs),
    /// Manage a marker-delimited span of a text file.
    Region(RegionArgs),
}

#[derive(Args)]
struct Common {
    /// Target file. Defaults to standard input.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,

    /// Write the result back to --file atomically instead of to standard output.
    #[arg(long, requires = "file")]
    in_place: bool,

    /// Report only. Writes nothing, to the file or anywhere else.
    #[arg(long)]
    check: bool,

    /// Also list the items that were already in place.
    #[arg(long)]
    verbose: bool,

    /// Overwrite values this tool cannot show it wrote, discarding them.
    #[arg(long)]
    force: bool,

    /// Remove items this tool wrote that are no longer declared.
    #[arg(long)]
    retract: bool,

    /// Where to keep the record of what this tool has written.
    #[arg(long, value_name = "PATH")]
    prior_store: Option<PathBuf>,
}

#[derive(Args)]
struct MergeArgs {
    /// Dotted path in the target, such as `env` or `permissions.deny`.
    /// A leading dot is tolerated.
    #[arg(long, value_name = "PATH")]
    key: String,

    /// JSON document to declare from.
    #[arg(long, value_name = "PATH")]
    fragment: PathBuf,

    /// Path inside the fragment. Defaults to --key, so a fragment can keep the
    /// same shape as the target and carry extra keys alongside.
    #[arg(long, value_name = "PATH")]
    fragment_key: Option<String>,

    /// Use the whole fragment document as the declared value.
    #[arg(long, conflicts_with = "fragment_key")]
    fragment_root: bool,

    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
struct RegionArgs {
    /// Marker name. A long-lived contract — renaming it orphans every file
    /// already carrying the old name.
    #[arg(long, value_name = "NAME")]
    marker: String,

    /// File holding the body, or `-` for standard input.
    #[arg(long, value_name = "PATH")]
    body: PathBuf,

    /// Comment syntax for the marker lines.
    #[arg(long, value_enum, default_value_t = Comment::Html)]
    comment: Comment,

    /// What to edit instead, named in the marker, such as `config/profile.md`.
    #[arg(long, value_name = "PATH")]
    source: Option<String>,

    /// Append the region when the marker is absent.
    #[arg(long)]
    init: bool,

    #[command(flatten)]
    common: Common,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Comment {
    /// `<!-- ... -->`, for markdown and html.
    Html,
    /// `# ...`, for shell, toml and yaml.
    Hash,
}

impl From<Comment> for MarkerStyle {
    fn from(c: Comment) -> Self {
        match c {
            Comment::Html => MarkerStyle::Html,
            Comment::Hash => MarkerStyle::Hash,
        }
    }
}

impl Common {
    fn options(&self) -> Options {
        Options {
            force: self.force,
            retract: self.retract,
        }
    }

    /// Whether this invocation actually applies the result to the target.
    ///
    /// Only then may provenance be recorded, since only then is there something
    /// to have a record of.
    fn applies(&self) -> bool {
        self.in_place && !self.check
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("cfggraft: {e}");
            ExitCode::from(EXIT_FAIL)
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    let (label, common, adapter) = match cli.cmd {
        Cmd::Merge(a) => {
            let common = a.common;
            let store = build_merge(
                &a.key,
                &a.fragment,
                a.fragment_key.as_deref(),
                a.fragment_root,
                &common,
            )?;
            ("merge", common, Adapter::Json(store))
        }
        Cmd::Region(a) => {
            let common = a.common;
            let store = build_region(
                &a.marker,
                &a.body,
                a.comment,
                a.source.as_deref(),
                a.init,
                &common,
            )?;
            ("region", common, Adapter::Marker(store))
        }
    };

    let mut adapter = adapter;
    let (report, output) = engine::run(&mut adapter, common.options())?;

    print_report(label, &report, common.verbose);

    if !common.check {
        if common.in_place {
            let path = common
                .file
                .as_ref()
                .ok_or_else(|| Error::invalid("--in-place needs --file"))?;
            // Leave the file alone when nothing changed, rather than rewriting
            // identical bytes and moving its modification time.
            if report.changed() {
                atomic::write(path, &output.document)?;
            }
        } else {
            io::stdout()
                .write_all(output.document.as_bytes())
                .map_err(|e| Error::io("<stdout>", e))?;
        }
    }

    if let (Some(owned), Some(path), true) = (&output.prior, &common.file, common.applies()) {
        let store_path = common
            .prior_store
            .clone()
            .unwrap_or_else(prior::Store::default_path);
        let mut store = prior::Store::open(store_path);
        store.set(path, owned);
        store.save()?;
    }

    Ok(if report.has_drift() {
        EXIT_DRIFT
    } else {
        EXIT_OK
    })
}

fn build_merge(
    key: &str,
    fragment: &Path,
    fragment_key: Option<&str>,
    fragment_root: bool,
    common: &Common,
) -> Result<JsonDoc> {
    let target_text = read_target(&common.file)?;
    let frag_text = fs::read_to_string(fragment).map_err(|e| Error::io(fragment, e))?;
    let fragment_doc: Value =
        serde_json::from_str(&frag_text).map_err(|e| Error::parse(fragment, e))?;

    let key_segs = json::split_path(key);
    let frag_segs = if fragment_root {
        Vec::new()
    } else {
        json::split_path(fragment_key.unwrap_or(key))
    };
    let declaration = json::get_path(&fragment_doc, &frag_segs).ok_or_else(|| {
        Error::invalid(format!(
            "{} declares nothing at `{}`",
            fragment.display(),
            json::display_path(&frag_segs)
        ))
    })?;

    let prior = load_prior(common);
    let name = common
        .file
        .clone()
        .unwrap_or_else(|| PathBuf::from("<stdin>"));
    JsonDoc::new(&target_text, &name, declaration, &key_segs, prior)
}

fn build_region(
    marker: &str,
    body: &Path,
    comment: Comment,
    source: Option<&str>,
    init: bool,
    common: &Common,
) -> Result<MarkerSpan> {
    let from_stdin = body == Path::new("-");
    if from_stdin && common.file.is_none() {
        return Err(Error::invalid(
            "--body - reads standard input, so the target needs --file",
        ));
    }
    let target_text = read_target(&common.file)?;
    let body_text = if from_stdin {
        read_stdin()?
    } else {
        fs::read_to_string(body).map_err(|e| Error::io(body, e))?
    };
    MarkerSpan::new(
        &target_text,
        marker,
        &body_text,
        comment.into(),
        source,
        init,
    )
}

/// What a previous run recorded for this target.
///
/// Empty without a target path, since there is nothing to key the record on.
/// That leaves every item unclaimed, which the decision table handles by
/// degrading to add-only.
fn load_prior(common: &Common) -> Vec<cfggraft::port::Item> {
    let Some(path) = &common.file else {
        return Vec::new();
    };
    let store_path = common
        .prior_store
        .clone()
        .unwrap_or_else(prior::Store::default_path);
    prior::Store::open(store_path).items(path)
}

fn read_target(file: &Option<PathBuf>) -> Result<String> {
    match file {
        Some(p) => match fs::read_to_string(p) {
            Ok(s) => Ok(s),
            // A target that does not exist yet is an empty document, not a
            // failure: the first run has to be able to create it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(Error::io(p, e)),
        },
        None => read_stdin(),
    }
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| Error::io("<stdin>", e))?;
    Ok(s)
}

/// Print one line per item, then a summary.
///
/// Items already in the declared state are listed only with `--verbose`: a run
/// over a settled declaration would otherwise bury the one line that matters.
fn print_report(label: &str, report: &Report, verbose: bool) {
    for e in &report.entries {
        if e.decision == Decision::Skip && !verbose && !suppressed_retraction(e) {
            continue;
        }
        eprintln!("{}", line_for(e));
    }

    let t = report.tally();
    let stale = report
        .entries
        .iter()
        .filter(|e| suppressed_retraction(e))
        .count();
    eprintln!(
        "{label}: added={} updated={} skipped={} retracted={} refused={}",
        t.added, t.updated, t.skipped, t.retracted, t.refused
    );

    if stale > 0 {
        eprintln!("  {stale} item(s) are no longer declared; pass --retract to remove them.");
    }
    if t.refused > 0 {
        eprintln!("  Nothing was overwritten. Pass --force to overwrite anyway.");
    }
}

/// Whether this entry is a retraction the caller chose not to perform.
fn suppressed_retraction(e: &Entry) -> bool {
    e.decision == Decision::Skip && e.overridden
}

fn line_for(e: &Entry) -> String {
    let label = if suppressed_retraction(e) {
        "stale"
    } else {
        e.decision.label()
    };
    let mut line = format!("{label:<11} {}", e.addr);

    match e.decision {
        Decision::Update => {
            if let (cfggraft::port::Observed::Present(o), Some(d)) = (&e.observed, &e.declared) {
                line.push_str(&format!(" {} -> {}", o.truncated(40), d.truncated(40)));
            }
        }
        Decision::Add | Decision::Retract => {
            if let Some(d) = value_of(e) {
                line.push_str(&format!(" {}", d.truncated(60)));
            }
        }
        Decision::Refuse(_) => {
            if let cfggraft::port::Observed::Present(o) = &e.observed {
                line.push_str(&format!(": kept {}", o.truncated(40)));
            }
            if let Some(d) = &e.declared {
                line.push_str(&format!(", declared {}", d.truncated(40)));
            }
        }
        Decision::Skip => {
            if let Some(d) = value_of(e) {
                line.push_str(&format!(" {}", d.truncated(60)));
            }
        }
    }

    if let Some(note) = &e.note {
        line.push_str(&format!(" ({note})"));
    }
    if e.overridden && !suppressed_retraction(e) {
        line.push_str(" (forced)");
    }
    line
}

/// The value worth showing for an entry: what is declared, or failing that what
/// is there.
fn value_of(e: &Entry) -> Option<&cfggraft::port::Repr> {
    match (&e.declared, &e.observed) {
        (Some(d), _) => Some(d),
        (None, cfggraft::port::Observed::Present(o)) => Some(o),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfggraft::port::{Addr, Observed, Repr};
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn entry(decision: Decision, overridden: bool) -> Entry {
        Entry {
            addr: Addr::new("env.A"),
            decision,
            declared: Some(Repr::new("\"2\"")),
            observed: Observed::Present(Repr::new("\"1\"")),
            note: None,
            overridden,
        }
    }

    #[test]
    fn an_update_line_shows_both_values() {
        let line = line_for(&entry(Decision::Update, false));
        assert!(line.contains("env.A"), "got: {line}");
        assert!(line.contains("\"1\" -> \"2\""), "got: {line}");
    }

    #[test]
    fn a_refusal_line_says_what_was_kept() {
        let line = line_for(&entry(
            Decision::Refuse(cfggraft::policy::Reason::Conflict),
            false,
        ));
        assert!(line.starts_with("conflict"), "got: {line}");
        assert!(line.contains("kept \"1\""), "got: {line}");
        assert!(line.contains("declared \"2\""), "got: {line}");
    }

    #[test]
    fn a_forced_line_says_so() {
        let line = line_for(&entry(Decision::Update, true));
        assert!(line.ends_with("(forced)"), "got: {line}");
    }

    #[test]
    fn a_suppressed_retraction_reads_as_stale_not_forced() {
        let e = Entry {
            declared: None,
            ..entry(Decision::Skip, true)
        };
        assert!(suppressed_retraction(&e));
        let line = line_for(&e);
        assert!(line.starts_with("stale"), "got: {line}");
        assert!(!line.contains("forced"), "got: {line}");
    }
}
