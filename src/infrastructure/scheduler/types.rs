//! Common types shared across the scheduler module.
//!
//! Nothing here talks to the DB or the async runtime — pure data
//! definitions so downstream modules (handler, registry, engine) can
//! import without dragging in transitive dependencies. See
//! `docs/plan/job-registry.md` Part 1 for the design rationale.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Per-dispatch parameter values, keyed by the names the job declared
/// in [`JobParam`], passed into
/// [`JobHandler::run`](super::handler::JobHandler::run).
///
/// This carried four fixed fields — `force`, `deep`, `storage`,
/// `repair` — plus a doc block enumerating what each meant for each
/// job, ending in "Others — ignored". That list is gone: the semantics
/// now live on each job's own [`JobParam::description`], next to the
/// code that reads them, where they cannot drift out of date. A job
/// that ignores a parameter no longer *has* it.
///
/// A map rather than a struct because the four fixed fields were
/// hardcoded in six places and silently dropped anything new — see
/// [`JobParam`] for the full story.
///
/// **The engine seeds this from the job's declared defaults before
/// overlaying caller values**, so a handler reading a parameter it
/// declared always finds it, of the right type. Reading a parameter the
/// job did NOT declare yields the accessor's fallback — which is a bug
/// in the job, and why `parameters()` and the reads should be edited
/// together.
#[derive(Debug, Clone, Default)]
pub struct JobRunArgs {
    values: std::collections::BTreeMap<String, JobParamValue>,
    /// True when nobody is watching this dispatch — the periodic tick,
    /// and anything else that fires without a human in the loop.
    ///
    /// NOT a `JobParam`, deliberately. Declared parameters are settable
    /// over the wire, and this one decides whether a paused repair-mode
    /// run may be continued (see `UNATTENDED_RESUME_PARAM`): a caller
    /// able to set it could hand itself the consent the guard exists to
    /// require. Keeping it off the declaration means
    /// [`JobRunArgs::from_declared`] rejects it as an unknown parameter,
    /// so the wire cannot reach it at all.
    ///
    /// Default `false` — "a human asked for this" — because every
    /// constructor but the tick's is reached from an operator action.
    /// The tick marks itself with [`JobRunArgs::unattended`].
    unattended: bool,
}

impl JobRunArgs {
    /// Build from already-parsed values. Callers that have raw wire
    /// strings should go through [`JobRunArgs::from_declared`] so the
    /// declaration does the parsing and validation.
    pub fn new(values: std::collections::BTreeMap<String, JobParamValue>) -> Self {
        Self {
            values,
            unattended: false,
        }
    }

    /// Mark this dispatch as unattended — no human in the loop.
    ///
    /// Called by the periodic tick only. Consumed by `run_or_resume` to
    /// decide whether a paused run may be continued without a fresh
    /// operator action.
    pub fn unattended(mut self) -> Self {
        self.unattended = true;
        self
    }

    /// True when no human is behind this dispatch. See [`Self::unattended`].
    pub fn is_unattended(&self) -> bool {
        self.unattended
    }

    /// Seed from `declared` defaults, then overlay `raw` wire values.
    ///
    /// This is the single place a caller's strings become typed values,
    /// shared by the trigger endpoint, the startup-jobs parser and the
    /// resume path — so all three accept exactly the same inputs and
    /// reject the same ones.
    ///
    /// An undeclared name is an error, not a silent drop: `?repare=true`
    /// on a job that mutates only under `repair` would otherwise run in
    /// discovery mode and report "nothing to do", which reads as success.
    pub fn from_declared<'a, I>(declared: &[JobParam], raw: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut values = std::collections::BTreeMap::new();
        for p in declared {
            values.insert(p.name.to_string(), p.default.to_value());
        }
        for (key, raw_value) in raw {
            let Some(p) = declared.iter().find(|p| p.name == key) else {
                return Err(if declared.is_empty() {
                    format!("unknown parameter '{key}': this job accepts none")
                } else {
                    format!(
                        "unknown parameter '{key}' (accepted: {})",
                        declared
                            .iter()
                            .map(|p| p.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                });
            };
            values.insert(p.name.to_string(), p.parse_value(raw_value)?);
        }
        Ok(Self {
            values,
            unattended: false,
        })
    }

    /// Reshape to exactly `declared`: every declared parameter present,
    /// seeded from its default unless this map already carries it, and
    /// anything undeclared dropped.
    ///
    /// **Applied by `dispatch` to every run**, which is what makes
    /// "a handler always sees its declared parameters, with the right
    /// defaults" true rather than merely usual. Three callers otherwise
    /// bypass the typed constructors and would each be a hole:
    ///
    /// * the periodic tick, which passes [`JobRunArgs::default()`] — an
    ///   EMPTY map, so a parameter declared with a non-`false` default
    ///   would silently read as `false` on every scheduled run;
    /// * programmatic triggers like [`JobRunArgs::with_string`], which
    ///   set one parameter and know nothing of the rest;
    /// * `consistency_batch`, which forwards its own args to sub-jobs
    ///   that declare a different set.
    ///
    /// Dropping rather than rejecting the undeclared is deliberate here:
    /// rejection belongs at the edge, where a human typed the name and
    /// can be told. By dispatch the value came from another job's
    /// declaration, and silently ignoring it is the whole point.
    pub fn normalized_for(&self, declared: &[JobParam]) -> Self {
        let mut values = std::collections::BTreeMap::new();
        for p in declared {
            let value = self
                .values
                .get(p.name)
                .cloned()
                .unwrap_or_else(|| p.default.to_value());
            values.insert(p.name.to_string(), value);
        }
        // Provenance survives normalisation. `dispatch` normalises EVERY
        // run, so dropping it here would silently un-mark the periodic
        // tick and hand the guard a run that looks operator-initiated.
        Self {
            values,
            unattended: self.unattended,
        }
    }

    /// One string parameter — the shape the storage-scoped programmatic
    /// triggers use (`backend_migration`, `backend_rotate`), which know
    /// their target and bypass the query-string path.
    pub fn with_string(name: &str, value: impl Into<String>) -> Self {
        let mut values = std::collections::BTreeMap::new();
        values.insert(name.to_string(), JobParamValue::String(Some(value.into())));
        Self {
            values,
            unattended: false,
        }
    }

    /// A declared boolean, or `false` when absent.
    pub fn get_bool(&self, name: &str) -> bool {
        match self.values.get(name) {
            Some(JobParamValue::Boolean(b)) => *b,
            _ => false,
        }
    }

    /// A declared string, or `None` when absent or empty.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(JobParamValue::String(Some(s))) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        }
    }

    /// A declared number, or `fallback` when absent.
    pub fn get_number(&self, name: &str, fallback: i64) -> i64 {
        match self.values.get(name) {
            Some(JobParamValue::Number(n)) => *n,
            _ => fallback,
        }
    }

    /// Every value, for the engine's persist path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &JobParamValue)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// True when nothing was supplied — used to keep log lines quiet
    /// for the common no-parameter dispatch.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Uniform outcome the supervisor logs and stores for every job dispatch.
///
/// Two variants, deliberately. Distinguishing *why* a job failed
/// (handler returned Err, `tokio::time::timeout` tripped,
/// `catch_unwind` caught a panic) is a **diagnostic** concern — it
/// belongs in a `cause` tracing field the supervisor sets, not in a
/// control-flow branch every consumer of `match outcome` has to
/// think about. See `docs/plan/job-registry.md` Part 1 §JobOutcome.
///
/// `Ok::count` is the row/record count the job reports as its primary
/// scalar (rows scanned, blobs migrated, thumbnails checked). `extra`
/// is a free-form JSON blob for job-specific fields the caller wants
/// surfaced to `oxicloud::scheduler` log lines.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobOutcome {
    Ok {
        count: u64,
        /// Free-form per-job payload, so `Object` is the honest schema —
        /// its keys are chosen by each handler (`purged`,
        /// `repaired_count`, `per_check`, …) and the admin panel reads
        /// the ones it knows. See `AdminJobsPanel`'s `extra` cast for
        /// the set that is load-bearing today.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        #[schema(value_type = Object)]
        extra: serde_json::Value,
    },
    /// `Err` is a struct variant (not tuple-newtype) so it composes
    /// with `#[serde(tag = "outcome")]`. Serde's internal tagging
    /// refuses to serialise a tuple variant wrapping a bare String
    /// — the tag has nowhere to live. The struct form `{ message }`
    /// lets serde emit `{"outcome":"err","message":"..."}` cleanly.
    Err { message: String },
}

impl JobOutcome {
    /// Ok with no extras — the common case for jobs that only report a count.
    pub fn ok(count: u64) -> Self {
        JobOutcome::Ok {
            count,
            extra: serde_json::Value::Null,
        }
    }

    /// Ok with a JSON `extra` payload. Use `serde_json::json!({...})`
    /// at call sites for readability.
    pub fn ok_with(count: u64, extra: serde_json::Value) -> Self {
        JobOutcome::Ok { count, extra }
    }

    /// Convenience constructor for `Err` — call-site ergonomics
    /// match the retired tuple form.
    pub fn err(message: impl Into<String>) -> Self {
        JobOutcome::Err {
            message: message.into(),
        }
    }

    /// Terse discriminant for logs / metrics: `"ok"` | `"err"`.
    pub fn kind(&self) -> &'static str {
        match self {
            JobOutcome::Ok { .. } => "ok",
            JobOutcome::Err { .. } => "err",
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, JobOutcome::Ok { .. })
    }
}

/// Diagnostic reason the supervisor attaches to the `cause` tracing
/// field when a job's outcome is [`JobOutcome::Err`]. Never persisted
/// as a first-class column — it's a log field only.
///
/// Handlers never construct this; the supervisor derives it from
/// which failure path fired:
/// - [`ErrCause::Handler`] — the handler returned `Err(_)` itself.
/// - [`ErrCause::Timeout`] — `tokio::time::timeout` tripped on the
///   registered `ScheduledJob.timeout` wall-clock cap.
/// - [`ErrCause::Panicked`] — `JoinHandle` returned a panic error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCause {
    Handler,
    Timeout,
    Panicked,
}

impl ErrCause {
    /// Stable label for the `cause` tracing field. Log aggregators key
    /// on these — renaming here IS a breaking change to any dashboard
    /// filtering on `cause = "handler"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrCause::Handler => "handler",
            ErrCause::Timeout => "timeout",
            ErrCause::Panicked => "panicked",
        }
    }
}

impl fmt::Display for ErrCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// When a job changes state.
///
/// Drives how the admin UI presents a trigger: `Never` earns a read-only
/// badge, `OnRepairOnly` is safe to run and warns only when the toggle is on,
/// `Always` warns regardless.
///
/// Three values rather than a boolean because there are three cases, and the
/// interesting one is conditional. `false` on a job that can delete files
/// under `?repair=true` is actively misleading; `true` on one that is
/// read-only by default is equally wrong. `OnRepairOnly` names the case a
/// boolean cannot, and it is where the recovery framework is heading —
/// discovery-only by default, mutation behind an explicit opt-in — so a
/// consistency tenant that later grows a repair arm changes this one value
/// and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mutates {
    /// Read-only under every flag. All consistency tenants.
    Never,
    /// Changes state on a plain run. GC, janitors, the import jobs.
    Always,
    /// Read-only by default; mutates only under `?repair=true`. Pairing this
    /// with `repair_description() == None` is contradictory — a job claiming
    /// it mutates only under a flag it does not support.
    OnRepairOnly,
}

/// The type of a declared job parameter, and the shape its value takes
/// on the wire.
///
/// Three types because that is what the query string and the admin
/// panel can express between them: a checkbox, a text/select input, a
/// number input. Anything richer belongs in the job's own config, not
/// in a per-run parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobParamType {
    Boolean,
    String,
    Number,
}

/// A parameter's declared default.
///
/// Separate from [`JobParamValue`] so [`JobParam`] contains no `String`
/// and stays const-constructible: a `&'static [JobParam]` literal in a
/// `parameters()` body needs const promotion, which a type with a
/// destructor blocks.
///
/// `Str` holds a `&'static str`, which is `Copy` with no destructor, so
/// the const-promotion argument above still holds.
///
/// It was omitted at first on the grounds that "a string parameter that
/// wants a default is usually config in disguise" — true of `storage`,
/// where `Absent` genuinely means *use the active backend*. `rederive` is
/// the counter-example that earned this variant: `changed` is a real
/// per-run default, named in the parameter's own description and
/// implemented by the job, so declaring `Absent` made the admin panel
/// render "no default" directly above a description reading
/// "'changed' (default)".
/// On the wire, `untagged` makes this `boolean | number | string | null`
/// — `Absent` is a unit variant, so it serialises as `null`. The schema
/// says exactly that rather than naming the Rust variants, because a
/// client sees values, never variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
#[schema(as = JobParamDefault)]
pub enum JobParamDefault {
    Boolean(bool),
    Number(i64),
    #[schema(value_type = String)]
    Str(&'static str),
    /// No default — the parameter is simply absent unless supplied.
    Absent,
}

impl JobParamDefault {
    /// The runtime value this default seeds a run with.
    pub fn to_value(self) -> JobParamValue {
        match self {
            Self::Boolean(b) => JobParamValue::Boolean(b),
            Self::Number(n) => JobParamValue::Number(n),
            Self::Str(s) => JobParamValue::String(Some(s.to_string())),
            Self::Absent => JobParamValue::String(None),
        }
    }
}

/// A value for a declared parameter, as supplied for one run.
///
/// `String` is `Option` because an absent string and an empty one are
/// different for `storage` — absent means "use the active backend",
/// empty would be a nameless entry.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum JobParamValue {
    Boolean(bool),
    String(Option<String>),
    Number(i64),
}

impl JobParamValue {
    /// The parameter type this value inhabits — used to reject a
    /// caller who sends `?deep=7` for a boolean.
    pub fn param_type(&self) -> JobParamType {
        match self {
            Self::Boolean(_) => JobParamType::Boolean,
            Self::String(_) => JobParamType::String,
            Self::Number(_) => JobParamType::Number,
        }
    }

    /// Render for the `params` JSONB column, which is `TEXT`-valued so
    /// a resumed run can restore whatever the fresh run was given.
    pub fn to_param_string(&self) -> Option<String> {
        match self {
            Self::Boolean(b) => Some(b.to_string()),
            Self::Number(n) => Some(n.to_string()),
            Self::String(s) => s.clone(),
        }
    }
}

/// One parameter a job accepts on a run.
///
/// # Why jobs declare these
///
/// The four parameters `force` / `deep` / `repair` / `storage` used to
/// be a fixed struct, and six places hardcoded that same list: the
/// engine's persist/restore, the trigger endpoint's query type, the
/// `OXICLOUD_JOBS_STARTUP` parser, the frontend API wrapper, and the
/// admin panel's checkboxes. Adding a parameter meant editing all of
/// them, and forgetting one meant the parameter was silently dropped —
/// most damagingly by the persist/restore path, where a resumed run
/// would quietly lose it.
///
/// Worse for operators: the panel showed the same knobs on every job.
/// Only two jobs read `deep` and six read `repair`, so most of those
/// checkboxes did nothing, with no way to tell which.
///
/// Now each job declares what it accepts. The engine iterates the
/// declaration, the trigger endpoint rejects anything undeclared, and
/// the panel renders exactly the knobs that job reads.
///
/// # Wire names are a compatibility surface
///
/// `name` is what `params` rows are keyed by and what the panel
/// switches on, so renaming one breaks existing run history the same
/// way renaming a [`Mutates`] variant would. Add a new parameter
/// rather than repurposing an old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct JobParam {
    #[schema(value_type = String)]
    pub name: &'static str,
    #[serde(rename = "type")]
    pub param_type: JobParamType,
    /// Applied when the caller omits the parameter. The engine seeds
    /// every run's args from these before overlaying caller values, so
    /// a handler reading a declared parameter always finds it.
    pub default: JobParamDefault,
    /// One line for the panel's input label. Empty renders bare.
    #[serde(skip_serializing_if = "str::is_empty")]
    #[schema(value_type = Option<String>)]
    pub description: &'static str,
    /// The only values this parameter accepts, when it is an enum in all
    /// but type. Empty means "any string" — the common case.
    ///
    /// Two jobs at once:
    ///
    /// * the admin panel renders a `<select>` instead of a text box, so
    ///   `alwyas` is not typeable;
    /// * [`JobParam::parse_value`] rejects an unlisted value, which makes
    ///   a typo a **400 at the trigger** rather than a dispatched run that
    ///   fails on its first statement.
    ///
    /// Still `JobParamType::String` on the wire — an enum value *is* a
    /// string, so nothing in type-checking or `JobParamValue` changes.
    ///
    /// `value_type = Option<Vec<String>>` because the serde attribute
    /// above omits the field when empty: a generated client must read
    /// "absent" as "any string", which is exactly the distinction the
    /// panel's text-box-vs-`<select>` branch turns on.
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    #[schema(value_type = Option<Vec<String>>)]
    pub allowed: &'static [&'static str],
}

impl JobParam {
    /// A boolean parameter, e.g. `?deep=true`.
    pub const fn boolean(name: &'static str, default: bool, description: &'static str) -> Self {
        Self {
            name,
            param_type: JobParamType::Boolean,
            default: JobParamDefault::Boolean(default),
            description,
            allowed: &[],
        }
    }

    /// A string parameter with no default, e.g. `?storage=azurite`.
    ///
    /// No `default` argument: a string parameter that wants one is
    /// almost always a config value in disguise. `storage` — the only
    /// string parameter today — means "the active backend" when absent,
    /// which is a job-side decision, not a default the engine can seed.
    pub const fn string(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            param_type: JobParamType::String,
            default: JobParamDefault::Absent,
            description,
            allowed: &[],
        }
    }

    /// A string parameter restricted to a fixed set, with a default that
    /// must be one of them.
    ///
    /// Use this wherever a string is really an enum: the panel renders a
    /// `<select>` and `parse_value` turns a typo into a 400 before the run
    /// is dispatched. `rederive` on `content_rescan` is the case it was
    /// added for — three modes, one of them (`always`) the only way to
    /// apply a fixed extractor.
    ///
    /// ⚠ `default` is not checked against `allowed` here: `const fn`
    /// cannot compare strings. The drift guard in this module's tests
    /// does it instead, for every declared parameter.
    pub const fn enumerated(
        name: &'static str,
        allowed: &'static [&'static str],
        default: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            param_type: JobParamType::String,
            default: JobParamDefault::Str(default),
            description,
            allowed,
        }
    }

    /// A numeric parameter, e.g. `?batch_size=500`.
    pub const fn number(name: &'static str, default: i64, description: &'static str) -> Self {
        Self {
            name,
            param_type: JobParamType::Number,
            default: JobParamDefault::Number(default),
            description,
            allowed: &[],
        }
    }

    /// Parse a wire value (query string / `OXICLOUD_JOBS_STARTUP` /
    /// restored `params` row) according to this parameter's type.
    ///
    /// Returns `Err` with an operator-facing reason rather than
    /// defaulting, so `?deep=yes` fails loudly instead of running a
    /// shallow scan the caller did not ask for.
    pub fn parse_value(&self, raw: &str) -> Result<JobParamValue, String> {
        match self.param_type {
            JobParamType::Boolean => match raw {
                // Deliberately strict — same rule as axum's `Query`
                // bool. "yes"/"1"/"on" are the shapes an operator
                // reaches for, and silently accepting them here while
                // the query layer rejects them would be worse than
                // rejecting both.
                "true" => Ok(JobParamValue::Boolean(true)),
                "false" => Ok(JobParamValue::Boolean(false)),
                other => Err(format!(
                    "'{other}' is not a boolean for parameter '{}' (use true or false)",
                    self.name
                )),
            },
            JobParamType::String if !self.allowed.is_empty() => {
                // The 400 that makes a typo cheap. Without this the value
                // reaches the job, which parses it there and fails the run
                // — so the operator learns about `alwyas` from a failed
                // run's error instead of from the request that made it.
                if self.allowed.contains(&raw) {
                    Ok(JobParamValue::String(Some(raw.to_string())))
                } else {
                    Err(format!(
                        "'{raw}' is not a valid value for parameter '{}' (expected one of: {})",
                        self.name,
                        self.allowed.join(", ")
                    ))
                }
            }
            JobParamType::String => Ok(JobParamValue::String(Some(raw.to_string()))),
            JobParamType::Number => raw
                .parse::<i64>()
                .map(JobParamValue::Number)
                .map_err(|_| format!("'{raw}' is not a number for parameter '{}'", self.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARED: &[JobParam] = &[
        JobParam::boolean("repair", false, "d"),
        JobParam::boolean("deep", true, "d"),
        JobParam::string("storage", "d"),
        JobParam::number("batch", 500, "d"),
        JobParam::enumerated("mode", &["changed", "always", "never"], "changed", "d"),
    ];

    #[test]
    fn an_enum_rejects_an_unlisted_value_before_the_run_starts() {
        let mode = DECLARED.iter().find(|p| p.name == "mode").unwrap();
        assert_eq!(
            mode.parse_value("always").unwrap(),
            JobParamValue::String(Some("always".to_owned()))
        );
        // ⚠ The whole point: a typo is a parse failure the caller sees,
        // not a dispatched run that dies on its first statement.
        let err = mode.parse_value("alwyas").unwrap_err();
        assert!(
            err.contains("alwyas"),
            "the error must quote the bad value: {err}"
        );
        assert!(
            err.contains("changed, always, never"),
            "the error must list the valid values: {err}"
        );
        // A free-form string parameter keeps taking anything.
        let storage = DECLARED.iter().find(|p| p.name == "storage").unwrap();
        assert!(storage.parse_value("whatever-entry").is_ok());
    }

    #[test]
    fn a_string_default_seeds_the_run_like_any_other() {
        let args = JobRunArgs::from_declared(DECLARED, []).unwrap();
        // The bug this variant fixes: with `Absent` the panel rendered
        // "no default" above a description naming one.
        assert_eq!(args.get_str("mode"), Some("changed"));
    }

    #[test]
    fn declared_defaults_seed_the_run() {
        let args = JobRunArgs::from_declared(DECLARED, []).unwrap();
        assert!(!args.get_bool("repair"));
        // Not merely "absent reads as false" — a declared `true` default
        // must survive, which is the whole reason defaults live in the
        // declaration rather than at each read site.
        assert!(args.get_bool("deep"));
        assert_eq!(args.get_str("storage"), None);
        assert_eq!(args.get_number("batch", 0), 500);
    }

    #[test]
    fn caller_values_overlay_defaults() {
        let args =
            JobRunArgs::from_declared(DECLARED, [("repair", "true"), ("deep", "false")]).unwrap();
        assert!(args.get_bool("repair"));
        assert!(!args.get_bool("deep"));
    }

    /// The failure the whole declaration exists to prevent: a typo that
    /// silently leaves a destructive job in discovery mode.
    #[test]
    fn an_undeclared_parameter_is_rejected_and_names_the_real_ones() {
        let err = JobRunArgs::from_declared(DECLARED, [("repare", "true")]).unwrap_err();
        assert!(err.contains("repare"), "{err}");
        assert!(err.contains("repair"), "must name what IS accepted: {err}");
    }

    #[test]
    fn a_job_declaring_nothing_says_so() {
        let err = JobRunArgs::from_declared(&[], [("force", "true")]).unwrap_err();
        assert!(err.contains("accepts none"), "{err}");
    }

    /// Same strictness as the HTTP layer's bool parsing, so a value that
    /// works in `OXICLOUD_JOBS_STARTUP` works in the trigger URL.
    #[test]
    fn booleans_take_only_true_or_false() {
        let err = JobRunArgs::from_declared(DECLARED, [("repair", "yes")]).unwrap_err();
        assert!(err.contains("not a boolean"), "{err}");
        assert!(JobRunArgs::from_declared(DECLARED, [("repair", "false")]).is_ok());
    }

    #[test]
    fn numbers_must_parse() {
        assert!(JobRunArgs::from_declared(DECLARED, [("batch", "x")]).is_err());
        let args = JobRunArgs::from_declared(DECLARED, [("batch", "12")]).unwrap();
        assert_eq!(args.get_number("batch", 0), 12);
    }

    /// An empty string is not a storage entry. `get_str` folding it to
    /// `None` is what keeps `?storage=` from resolving to a nameless
    /// backend rather than the active one.
    #[test]
    fn an_empty_string_reads_as_absent() {
        let args = JobRunArgs::from_declared(DECLARED, [("storage", "")]).unwrap();
        assert_eq!(args.get_str("storage"), None);
    }

    /// Reading a parameter the job never declared is a bug in the job,
    /// and it fails closed rather than panicking — the accessor's
    /// fallback stands in.
    #[test]
    fn reading_an_undeclared_parameter_falls_back() {
        let args = JobRunArgs::from_declared(DECLARED, []).unwrap();
        assert!(!args.get_bool("nonexistent"));
        assert_eq!(args.get_number("nonexistent", 7), 7);
    }

    /// `dispatch` applies this to every run, so the periodic tick — which
    /// passes an EMPTY `JobRunArgs::default()` — still gets the declared
    /// defaults. Without it a `default: true` parameter would read as
    /// false on every scheduled run and only be right when an operator
    /// triggered by hand.
    #[test]
    fn normalizing_an_empty_args_applies_declared_defaults() {
        let args = JobRunArgs::default().normalized_for(DECLARED);
        assert!(args.get_bool("deep"), "declared default true must survive");
        assert!(!args.get_bool("repair"));
        assert_eq!(args.get_number("batch", 0), 500);
    }

    #[test]
    fn normalizing_keeps_supplied_values_and_drops_undeclared() {
        // As `consistency_batch` forwards: its own `force` reaching a
        // sub-job that declares no such thing.
        let forwarded = JobRunArgs::new(
            [
                ("repair".to_string(), JobParamValue::Boolean(true)),
                ("force".to_string(), JobParamValue::Boolean(true)),
            ]
            .into_iter()
            .collect(),
        );
        let args = forwarded.normalized_for(DECLARED);
        assert!(args.get_bool("repair"), "supplied value survives");
        assert!(
            !args.iter().any(|(k, _)| k == "force"),
            "an undeclared parameter must not reach the handler or its params row"
        );
    }

    #[test]
    fn mutates_serialises_snake_case() {
        // The admin UI switches on these strings — a rename is a breaking
        // change to the panel, not just to Rust callers.
        assert_eq!(serde_json::to_string(&Mutates::Never).unwrap(), "\"never\"");
        assert_eq!(
            serde_json::to_string(&Mutates::Always).unwrap(),
            "\"always\""
        );
        assert_eq!(
            serde_json::to_string(&Mutates::OnRepairOnly).unwrap(),
            "\"on_repair_only\""
        );
    }

    #[test]
    fn joboutcome_kind_label() {
        assert_eq!(JobOutcome::ok(0).kind(), "ok");
        assert_eq!(JobOutcome::err("boom").kind(), "err");
    }

    #[test]
    fn errcause_labels_stable() {
        assert_eq!(ErrCause::Handler.as_str(), "handler");
        assert_eq!(ErrCause::Timeout.as_str(), "timeout");
        assert_eq!(ErrCause::Panicked.as_str(), "panicked");
    }

    /// `dispatch` normalises EVERY run, so a `normalized_for` that rebuilt
    /// the struct without carrying the mark would silently un-flag the
    /// periodic tick — and the resume guard would see every tick as
    /// operator-initiated. Exactly the kind of hole the normalisation
    /// funnel was introduced to close, reopened one field later.
    #[test]
    fn normalisation_preserves_the_unattended_mark() {
        const DECLARED: &[JobParam] = &[JobParam::boolean("repair", false, "test flag")];

        assert!(
            JobRunArgs::default()
                .unattended()
                .normalized_for(DECLARED)
                .is_unattended()
        );
        assert!(
            !JobRunArgs::default()
                .normalized_for(DECLARED)
                .is_unattended(),
            "an operator-initiated run must not acquire the mark"
        );
    }

    /// The mark is not reachable from the wire: it is not a declared
    /// parameter, so the edge rejects it rather than letting a caller
    /// grant itself the consent the resume guard requires.
    #[test]
    fn the_unattended_mark_cannot_be_set_over_the_wire() {
        const DECLARED: &[JobParam] = &[JobParam::boolean("repair", false, "test flag")];

        let err = JobRunArgs::from_declared(DECLARED, [("unattended", "true")])
            .expect_err("undeclared parameter must be refused");
        assert!(err.contains("unattended"), "unexpected message: {err}");

        assert!(
            !JobRunArgs::from_declared(DECLARED, [("repair", "true")])
                .expect("declared")
                .is_unattended()
        );
    }
}
