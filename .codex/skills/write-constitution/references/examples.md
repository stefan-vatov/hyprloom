# Example CONSTITUTION.md Documents

<purpose>
Three example constitutions for different project types. Use these as few-shot
references when drafting. Each demonstrates the 6-section structure with different
tones and scopes.
</purpose>

## Contents

- Example 1: Library (dataparse)
- Example 2: Application (MedTrack)
- Example 3: Platform (Forgekit)

---

## Example 1: Library (dataparse)

<example type="library" name="dataparse" description="A data parsing library">

```markdown
# CONSTITUTION.md

## Preamble

dataparse exists because data ingestion is the most thankless part of every
data pipeline. We believe parsing should be boring, predictable, and invisible.
The best parser is one you never think about.

This project was born from frustration with parsers that handle the happy path
beautifully and explode on the first malformed row. We chose to build something
that handles the ugly data gracefully, because ugly data is the only kind that
exists in production.

## Founding Principles

1. **Corrupt data is normal data.** Every parser must handle malformed input
   without crashing. The question is never "will the data be dirty?" but
   "how will we handle it when it is?" This rejects the assumption that
   upstream data is trustworthy.

2. **Parse progressively, fail precisely.** Extract what you can, flag what
   you can't, and always tell the caller exactly which field on which row
   failed and why. This rejects both silent data loss and opaque error messages.

3. **Zero-config for common cases, full-config for edge cases.** CSV, JSON,
   and fixed-width should work with zero arguments. Obscure formats and custom
   delimiters are configuration, not code changes. This rejects both the
   "configure everything" and "convention only" extremes.

4. **No dependencies for core parsing.** The core library uses only the
   standard library. Optional integrations (pandas, polars) are extras.
   This rejects the node_modules approach to dependency management.

5. **Streaming by default.** Never load an entire file into memory unless
   explicitly asked. A 50GB CSV should parse with the same memory footprint
   as a 50KB one. This rejects convenience-first memory models.

6. **The API is the product.** Function signatures, error types, and return
   values are the user interface. Changing them is a breaking change, period.
   Internal refactoring is free; API changes are expensive.

## Growth Directives

- **More formats, same philosophy.** Expand format support (Parquet, Avro,
  TOML) but every new format must follow the same error-handling and
  streaming principles.
- **Toward composition.** Parsers should compose: chain a CSV parser into
  a validator into a transformer. Pipelines, not monoliths.
- **Performance as a feature.** Benchmark-driven optimization, but never
  at the cost of correctness. A fast parser that silently drops rows is
  worse than a slow one that reports them.

## Boundaries

- **Never become an ETL framework.** We parse. We don't transform, load,
  schedule, or orchestrate. That's someone else's job.
- **Never add network I/O.** We read from streams and file handles. Fetching
  data from URLs, APIs, or databases is out of scope.
- **Never sacrifice correctness for benchmarks.** If a performance optimization
  changes output for any edge case, it's a bug, not a trade-off.

## Tension Pairs

- **Correctness over performance** — but never at the cost of being unusable
  on large files. A correct parser that takes 10x longer is acceptable; one
  that can't finish is not.
- **Simplicity over completeness** — but never at the cost of silently dropping
  data. Better to parse 5 formats perfectly than 50 formats partially.
- **Stability over features** — but never at the cost of ignoring real user
  needs. We'd rather ship slowly than break existing users.
- **Explicit over implicit** — but never at the cost of requiring boilerplate
  for the 90% case. Sane defaults with escape hatches.

## Amendments

_No amendments yet. This constitution was ratified on 2025-03-15._

### Amendment Process

To propose an amendment:
1. Open an issue titled "CONSTITUTION: [proposed change]"
2. Describe what principle changes and WHY the current version is wrong
3. Provide a concrete example where the current constitution led to a bad decision
4. Amendments require maintainer consensus and a 2-week discussion period
```

</example>

---

## Example 2: Application (MedTrack)

<example type="application" name="medtrack" description="A medication tracking healthcare app">

```markdown
# CONSTITUTION.md

## Preamble

MedTrack exists because medication non-adherence kills 125,000 Americans
per year. Not from lack of medicine — from lack of a system that actually
works with how humans live. We build for the 80-year-old who can barely
see their phone, the caregiver managing 5 people's medications, and the
patient who just wants to stop worrying about whether they took their
morning dose.

This is not a productivity app. This is a safety system.

## Founding Principles

1. **Safety over convenience.** Every feature is evaluated first by "can this
   cause someone to miss or double a dose?" If yes, the feature needs guardrails
   before it ships. This rejects move-fast-and-break-things for anything
   touching medication data.

2. **Works offline, syncs when possible.** A patient's medication reminder
   must fire even in airplane mode, in rural areas with no signal, during
   outages. Cloud sync is a luxury; local reliability is a requirement.
   This rejects cloud-first architecture for critical paths.

3. **Accessible first, pretty second.** WCAG AAA on critical flows. Large
   touch targets. High contrast. Screen reader support is not an afterthought
   retrofit — it's designed in from day one. This rejects the "we'll add
   accessibility later" approach.

4. **The patient's data belongs to the patient.** Export everything, any time,
   in standard formats. No lock-in, no paywalls on your own health data.
   If a patient leaves MedTrack, their data leaves with them.

5. **Silence means something is wrong.** If a reminder doesn't fire, that's a
   critical bug, not a minor issue. The most dangerous failure mode is silent
   failure. Every critical path has heartbeat monitoring.

6. **Caregiver workflows are first-class.** A caregiver managing multiple
   patients is not an edge case — it's a primary persona. Multi-patient
   views, delegation, and notifications are core features, not add-ons.

7. **Clinical accuracy over engagement metrics.** We don't gamify medication
   adherence. No streaks, no points, no shame. We provide accurate information
   and reliable reminders. This rejects engagement-driven design for
   health-critical tools.

## Growth Directives

- **Toward clinical integration.** EHR integration, pharmacy sync, provider
  dashboards — but always with the patient in control of what's shared.
- **Toward proactive safety.** Interaction checking, refill prediction,
  anomaly detection — safety features that anticipate problems.
- **Toward family networks.** Expanding from single-caregiver to care teams
  with appropriate role-based access.

## Boundaries

- **Never become a social network.** No feeds, no community features, no
  "see how other patients manage their meds." This is a private health tool.
- **Never sell or share patient data.** Not to researchers, not to pharma
  companies, not to insurers. Not even anonymized, unless the patient
  explicitly opts in per-request.
- **Never require an account to use core features.** Medication reminders
  work without registration. Sync and sharing require an account; reminders don't.
- **Never deprioritize older devices.** If we drop support for an OS version,
  the app must continue to work offline for existing installs for 2+ years.

## Tension Pairs

- **Reliability over features** — but never at the cost of ignoring clinically
  important capabilities. Ship fewer features, but never ship unreliable ones.
- **Privacy over convenience** — but never at the cost of making the app
  unusable for cognitively impaired patients. Privacy walls must not become
  accessibility barriers.
- **Simplicity over power** — but never at the cost of blocking caregiver
  workflows. The primary patient view is simple; the caregiver view can be complex.
- **Caution over speed** — but never at the cost of letting a known safety
  issue persist. Slow to ship features, fast to fix safety bugs.

## Amendments

_No amendments yet. This constitution was ratified on 2025-01-20._

### Amendment Process

Safety-related amendments follow an expedited process (48-hour review).
All other amendments require:
1. A written proposal with the rationale and a concrete scenario
2. Clinical advisory review (for patient-safety-related changes)
3. Team vote with 2/3 majority
4. 30-day trial period before permanent adoption
```

</example>

---

## Example 3: Platform (Forgekit)

<example type="platform" name="forgekit" description="A developer platform/framework for building internal tools">

```markdown
# CONSTITUTION.md

## Preamble

Forgekit exists because every company eventually builds an internal tools
platform, and they all make the same mistakes. They either over-engineer a
generic platform that's painful to use, or they under-engineer a collection
of scripts that can't scale. We chose the middle path: opinionated enough
to be productive, flexible enough to not be a cage.

We build for the senior engineer who's been asked to "just build a quick
admin panel" for the fifth time and wants to stop reinventing CRUD.

## Founding Principles

1. **Convention over configuration, escape hatch over convention.** The
   default path should require zero config. But when the default doesn't
   fit, there must be a clean way to override it — not a hack, a documented
   escape hatch. This rejects both "configure everything" and "our way or
   the highway."

2. **Build for the 1000th internal tool, not the 1st.** Design decisions
   should optimize for the experience of maintaining many tools over years,
   not the experience of building one tool today. This rejects demo-driven
   development.

3. **Ugly but shipped beats beautiful but theoretical.** A working admin
   panel with default styling that's live today provides more value than a
   pixel-perfect design that's "almost ready." This rejects perfectionism
   as a shipping strategy.

4. **The platform is invisible.** Users of internal tools should never
   think about the framework. They think about their data, their workflows,
   their permissions. If a user says "the Forgekit app," something went wrong.
   This rejects framework-as-brand for internal tools.

5. **Upgrade paths are sacred.** A team with 50 Forgekit apps must be able
   to upgrade the framework without rewriting their apps. Breaking changes
   require migration tooling, not migration documentation. This rejects
   "just follow the upgrade guide" as a migration strategy.

6. **Security defaults, not security options.** Authentication, authorization,
   audit logging, and input sanitization are on by default. Turning them off
   requires explicit, auditable configuration. This rejects opt-in security
   for internal tools.

## Growth Directives

- **Toward self-service.** Non-engineers should be able to build simple tools
  (dashboards, forms, approval workflows) without writing code.
- **Toward observability.** Every Forgekit app should automatically surface
  usage metrics, error rates, and performance data to its owners.
- **Toward ecosystem.** A marketplace of reusable components (data sources,
  widgets, auth providers) contributed by teams across the organization.

## Boundaries

- **Never become a general-purpose web framework.** We build internal tools.
  Customer-facing apps, marketing sites, and public APIs are out of scope.
- **Never sacrifice upgrade safety for feature velocity.** A new capability
  is worthless if it breaks 50 existing apps.
- **Never require deep framework knowledge.** If using Forgekit requires
  understanding its internals, the abstraction has failed.
- **Never build what the ecosystem already solved.** Charting, rich text
  editing, date picking — use the best existing library, don't build our own.

## Tension Pairs

- **Productivity over flexibility** — but never at the cost of making
  common customizations impossible. 90% of use cases should be trivial;
  the remaining 10% should be possible.
- **Stability over innovation** — but never at the cost of falling behind
  security patches. The platform can be boring; it cannot be vulnerable.
- **Simplicity over power** — but never at the cost of forcing workarounds
  for legitimate needs. Simple API surface with deep capability.
- **Consistency over perfection** — but never at the cost of consistency
  with something that's wrong. A consistent bad pattern is still bad.

## Amendments

### Amendment 1 (2025-06-15): Security Defaults
**Changed:** Principle 6 was originally "Security is everyone's responsibility."
**Reason:** This was a Motherhood statement — nobody disagrees. The new version,
"Security defaults, not security options," provides actionable direction: things
are secure unless explicitly made insecure.

### Amendment Process
1. File an RFC in the forgekit-rfcs repository
2. Link to a specific decision where the current constitution gave wrong guidance
3. 2-week comment period
4. Core team vote (majority)
5. If approved, update this document with the amendment record above
```

</example>

---

<usage-guidance>

  <when-to-reference>Show these examples to calibrate tone and specificity during drafting</when-to-reference>
  <how-to-use>
    - Use the library example for open-source projects and developer tools
    - Use the application example for user-facing products, especially safety-critical ones
    - Use the platform example for frameworks, infrastructure, and multi-team systems
    - Point users to the example closest to their project type
    - Note how each example's Founding Principles reject specific alternatives
    - Note how Tension Pairs include the "but never at the cost of Z" clause
    - Note how Boundaries are philosophical, not operational
  </how-to-use>

</usage-guidance>
