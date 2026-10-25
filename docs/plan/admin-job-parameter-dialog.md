# Plan — A parameter dialog for job triggers

**Status:** proposed 2026-10-25, not started. Awaiting review.

The admin jobs panel can trigger a job with **one boolean set to true**. Every
other way a job can be parameterised is unreachable from the UI. The backend
already publishes everything a form needs, so this is a frontend-only change
plus one locale key set.

## What already works — do not rebuild it

The panel is already generic about booleans, and that was deliberate.
`AdminJobsPanel.svelte::extraBooleanParams` renders every declared boolean
beyond `deep` and `repair` as its own run-menu entry, so a newly-declared flag
becomes reachable with no frontend release. Its own comment records why:
`supportsDeep` used to be a hardcoded name allowlist (`consistency_batch ||
backend_consistency`), which meant a job gaining a deep mode needed a frontend
release to become reachable, and a job losing one left a menu item that
silently did nothing.

`content_rescan`'s new `repair_divergence` appeared in the menu with no
frontend work at all. **That path stays.** This plan does not replace the
one-click entries for `deep` and `repair`, which carry bespoke confirmation
copy (`repair_description`) and are the common case.

The API layer is generic too: `triggerJob(name, opts)` takes arbitrary
parameters and its doc already says *"Which parameters are valid is the job's
answer, not this function's — read them from `JobSummary.parameters`."*

And the backend already serialises what a form needs, per parameter:

```rust
pub struct JobParam {
    pub name: &'static str,
    pub param_type: JobParamType,   // Boolean | String | Number
    pub default: JobParamDefault,
    pub description: &'static str,  // "One line for the panel's input label"
}
```

That doc comment on `description` is from the original design. The field has
been shipped and serialised the whole time; the panel renders it as a `title=`
tooltip and nothing else.

## The three gaps

**1. String and number parameters have no UI at all.** `extraBooleanParams`
filters to `p.type === 'boolean'` by explicit decision — *"a generic text box
in a run menu would be a worse way to ask"*, which is right about a *menu* and
is the reason to use a dialog instead. Nine job services declare non-boolean
parameters today:

| job | unreachable parameters |
|---|---|
| `content_rescan` | `rederive`, `mime_prefix` |
| `notify_selftest` | 4 (2 string, 2 number) |
| `collab_idle_gc` | 2 |
| `backend_consistency`, `backend_migration`, `backend_rotate`, `drive_policies_consistency`, `notifications_cleanup`, `job_runs_cleanup` | 1 each |

This is not cosmetic. `content_rescan`'s `rederive=always` is **the only mode
that repairs a fixed extractor** — an extractor fix does not change the
sniffed type, so the default `changed` skips every affected row. The ID3v2.4
year fix needs it. An operator cannot ask for it from the admin panel.

Two of these already have bespoke frontend paths rather than generic ones:
`job_runs_cleanup`'s `retention_days` is hardcoded in
`adminJobs.ts`, and `storage` is passed contextually by the storage tab's
audit/migrate actions. Those are the duplication this removes.

**2. Parameters cannot be combined.** Each boolean is its own menu item
dispatching `{ [p.name]: true }`, so exactly one is ever set. A dry run *with*
a specific mode — `dry_run=true` **and** `rederive=always`, the obvious way to
preview an extractor backfill — cannot be expressed. `content_rescan` has four
parameters and the menu can reach one at a time.

**3. Defaults and descriptions are not visible.** `default` is never rendered;
`description` is a `title=` tooltip, so it is hover-only and invisible on
touch. An operator deciding whether `repair_divergence` is safe needs to read
that the filename is the weaker oracle — which is in the declaration, and which
no one will hover to find.

## Design

**Revised 2026-10-25 (Ed): the run menu items become presets that seed the
dialog the launch already shows.** Not a new "Run with parameters…" entry
beside them — the menu already reads `Run` / `Run dry run` / `Run with
repair_divergence`, and each of those is a *named set of parameter values*.
Clicking one opens the dialog with that set applied, and the operator adjusts
before confirming.

This is strictly better than a separate entry, for three reasons:

1. **It closes gap 2 for free.** Click "Run with repair_divergence", and the
   dialog opens with that box already ticked — then tick `dry_run` and set
   `rederive` too. The combination that was inexpressible becomes the default
   interaction, with no extra menu item to find.
2. **One dialog, not two.** A mutating job already shows a confirm on launch
   (`onTriggerGuarded`). Putting the parameters *in* that popup means the
   operator reads the warning and sets the values in the same step, instead of
   confirming and then being asked.
3. **The menu stays the discovery surface.** A newly-declared boolean still
   appears there on its own, which is what `extraBooleanParams` is for. It now
   seeds the dialog rather than dispatching blind.

### When the dialog does NOT open

It has to earn the click. Skip it and trigger straight through when the job
declares **no parameters** and `mutates === 'never'` — there is nothing to
show and nothing to warn about, and read-only jobs triggering in one click is
the point of that flag. Everything else opens it, including a `never` job that
declares parameters (otherwise its non-booleans stay unreachable, which is
gap 1).

### Where it lives

`dialogs.svelte.ts` cannot host this. It is a promise-based store with exactly
two kinds — `confirm` → `boolean` and `prompt` → `string | null` — rendered by
`<DialogHost>`, and both are app-wide primitives. A job parameter form is not
one, so adding a third `params` kind would push admin-specific shape into a
generic store.

So the dialog is its own component on the existing `Modal`:
`lib/components/admin/JobParamsDialog.svelte`, taking `JobSummary` plus the
preset, resolving to `JobParamValues | null`. `AdminJobsPanel` keeps ownership
of triggering; the dialog only collects values. That also makes it testable
without the panel's menu and busy-key machinery.

The form itself, built entirely from `JobSummary.parameters`:

```
Shown here seeded by the "Run with repair_divergence" menu item — note that
box is pre-ticked while everything else sits at its declared default:

┌─ Run content_rescan ─────────────────────────────────┐
│ Re-sniff stored content and correct its MIME type.   │
│                                                      │
│ ☐ dry_run                              default: off  │
│   Report what would change without writing. The      │
│   backend reads still happen — the cost of the run    │
│   is identical; only the UPDATEs are suppressed.      │
│                                                      │
│   rederive  [changed            ]      default: —     │
│   When to re-extract metadata after sniffing:        │
│   'changed' (default) only for content whose type…   │
│                                                      │
│ ☑ repair_divergence                    default: off  │
│   Converge copies of one content whose names imply…  │
│                                                      │
│   mime_prefix  [                ]      default: —     │
│   Restrict the walk to content currently stored…     │
│                                                      │
│ ⚠ This job changes stored state when it runs.        │
│                            [Cancel]  [Run]           │
└──────────────────────────────────────────────────────┘
```

Rules:

- **One widget per declared type.** `boolean` → checkbox seeded from
  `default`; `number` → `<input type="number">`; `string` → text input. No
  per-job knowledge anywhere in the component.
- **Descriptions render as body text**, not tooltips. They are written as one
  line for exactly this (`JobParam::description`'s doc says so), and several
  are already multi-sentence because the decision needs explaining.
- **Omitted means default — and a preset value is NOT "touched".** Only
  parameters the operator actually changed, plus the preset's own values, go
  into the query string. The engine seeds every run's args from the
  declarations before overlaying caller values, so `{}` is already correct,
  and sending a full set would pin today's defaults into run history as if
  they had been chosen. A box the preset ticked is a choice; a box the preset
  left alone is not, even though both were rendered.
- **The mutation warning stays**, inline. `mutates === 'always'` shows the
  copy `onTriggerGuarded` uses today, and `repair` ticked inside the dialog
  must surface `repair_description` — the only destructive-click wording the
  backend owns, and the reason the one-click repair entry exists at all. It
  appears in the dialog rather than as a second confirm after it.
- **The confirm button is the trigger.** No separate confirm step, because the
  dialog *is* the confirm. Cancel must dispatch nothing.

### Where it lives

`AdminJobsPanel.svelte` is already large. The dialog goes in its own component
— `lib/components/admin/JobParamsDialog.svelte` — taking a `JobSummary` and
resolving to `JobParamValues | null`. The panel keeps ownership of triggering;
the dialog only collects values. That also makes it testable without the
panel's menu/busy-key machinery.

### Tests

- **Vitest** on the value-collection logic, which is where the subtle rules
  are: untouched parameters absent from the result; a parameter the *preset*
  set present even when it equals its declared default; a box the preset left
  alone still absent; a touched boolean present even when toggled back to its
  default value; a cleared text input absent rather than `""`.
- **Vitest** on the skip rule: a job with no parameters and `mutates: never`
  must trigger without opening anything, and a `never` job that declares
  parameters must still open.
- **One Playwright case** — `rederive=always` reaching the wire — since the
  whole point is a parameter that no other path can express.
- No Rust change, so no Hurl.

## Not in scope

- **Changing `JobParam`.** It already carries name, type, default and
  description. If a parameter needs an enum of allowed values (`rederive` is
  `changed|always|never`, and a free-text box will let an operator type
  `alwyas` — which the job correctly rejects, but only after they click Run),
  that is a *backend* change adding `allowed: &'static [&'static str]` and
  deserves its own decision. Worth noting the job already pins this:
  `rederive_rejects_a_typo_by_name`.
- **Scheduling.** This dialog triggers now; it does not set intervals.
- **Removing the one-click `deep` / `repair` entries.** They stay.

## Open question

Does the dialog replace `job_runs_cleanup`'s hardcoded `retention_days` path
in `adminJobs.ts` immediately, or after it has shipped? Replacing it is the
point — it is the duplication AGENTS.md forbids — but it is also the one
parameterised trigger that works today, so it should be the last thing
removed, not the first.
