# Codebase Signal Detection

<purpose>
What to look for when scanning a codebase to seed Socratic questions. Signals are
observations, never conclusions. Each signal maps to a question, not an answer.
</purpose>

## Contents

- Signal Categories
- Signal-to-Question Mapping
- Exploration Checklist
- Anti-patterns in Signal Interpretation

---

## Signal Categories

<signals>

  <category name="project-identity">
    <signal source="README.md, package.json, mix.exs, Cargo.toml">
      Project description, tagline, and stated purpose. What does the project SAY it is?
    </signal>
    <signal source="LICENSE">
      License choice signals openness philosophy (MIT = permissive, AGPL = copyleft, proprietary = closed)
    </signal>
    <signal source="AGENTS.md, CLAUDE.md, CONTRIBUTING.md">
      Existing operational philosophy. Constitution should be ABOVE this level.
    </signal>
    <signal source="ADRs (docs/adr/, docs/decisions/)">
      Architecture Decision Records reveal past trade-off reasoning.
    </signal>
  </category>

  <category name="dependency-philosophy">
    <signal source="package.json, requirements.txt, mix.exs, Cargo.toml">
      Dependency count and type: few deps = self-reliant philosophy, many deps = ecosystem-leverage philosophy
    </signal>
    <signal source="vendor/ or copied source files">
      Vendoring signals distrust of dependency stability or desire for control
    </signal>
    <signal source="dependency age and maintenance status">
      Using old, stable deps vs. cutting-edge deps signals risk tolerance
    </signal>
  </category>

  <category name="code-structure">
    <signal source="directory layout">
      Flat = simplicity preference. Deep nesting = domain modeling preference.
      Monorepo = unified, polyrepo = independence.
    </signal>
    <signal source="naming conventions">
      Verbose names (getUserAccountBalanceHistory) = explicitness. Short names (get_bal) = terseness.
      Consistency of conventions across the codebase signals discipline priority.
    </signal>
    <signal source="abstraction layers">
      Many interfaces/protocols = flexibility priority. Concrete implementations = pragmatism priority.
    </signal>
  </category>

  <category name="error-handling">
    <signal source="error handling patterns">
      Exceptions vs. result types vs. error codes signals philosophy on failure handling.
      Panic/crash-early vs. graceful degradation signals fault tolerance philosophy.
    </signal>
    <signal source="logging patterns">
      Verbose logging = observability priority. Minimal logging = simplicity priority.
      Structured logging = operability priority.
    </signal>
  </category>

  <category name="testing-philosophy">
    <signal source="test/ directory structure and coverage">
      Unit-heavy = implementation confidence. Integration-heavy = behavior confidence.
      No tests = speed priority (or neglect). Property-based tests = correctness obsession.
    </signal>
    <signal source="test naming and style">
      BDD-style (describe/it) = behavior focus. Function-name tests = implementation focus.
    </signal>
    <signal source="CI configuration">
      Fast CI = developer velocity priority. Comprehensive CI = quality gate priority.
    </signal>
  </category>

  <category name="change-patterns">
    <signal source="commit message style">
      Conventional commits = process discipline. Freeform = flexibility.
      Long messages = documentation culture. Short messages = speed culture.
    </signal>
    <signal source="PR/merge patterns (if visible)">
      Required reviews = quality gate. Direct pushes = trust/speed.
      PR templates = structured communication priority.
    </signal>
    <signal source="changelog/release notes">
      Detailed changelogs = user communication priority. No changelog = internal focus.
    </signal>
  </category>

  <category name="documentation-culture">
    <signal source="inline comments density and style">
      Heavy comments = explanation priority. No comments = self-documenting code priority.
      TODO/FIXME/HACK comments = technical debt awareness.
    </signal>
    <signal source="docs/ directory">
      Rich docs = knowledge sharing priority. Minimal docs = code-as-docs philosophy.
    </signal>
    <signal source="README quality">
      Marketing-quality README = adoption priority. Terse README = internal tool mindset.
    </signal>
  </category>

</signals>

---

## Signal-to-Question Mapping

<mappings>

  <mapping>
    <signal>Few dependencies, some vendored</signal>
    <question>"The codebase has relatively few external dependencies and some are vendored. Is self-reliance an intentional value, or is this just how things evolved?"</question>
  </mapping>

  <mapping>
    <signal>Deep directory structure with domain-named modules</signal>
    <question>"The code is organized by domain concepts rather than technical layers. Is domain modeling a core philosophy, or would you restructure if starting fresh?"</question>
  </mapping>

  <mapping>
    <signal>Comprehensive CI with many check stages</signal>
    <question>"CI runs quite a few checks before merge. Do you see the quality gate as a core value, or is it something that grew organically and might be trimmed?"</question>
  </mapping>

  <mapping>
    <signal>Result types for error handling instead of exceptions</signal>
    <question>"The codebase uses explicit result types rather than exceptions. Is making failure visible a philosophical commitment, or a language convention you're following?"</question>
  </mapping>

  <mapping>
    <signal>Verbose, descriptive naming everywhere</signal>
    <question>"Names are quite descriptive throughout the codebase. Is explicitness a principle you'd enshrine, even at the cost of verbosity?"</question>
  </mapping>

  <mapping>
    <signal>Test coverage heavily weighted toward integration tests</signal>
    <question>"Testing leans toward integration tests over unit tests. Is testing behavior rather than implementation a deliberate philosophy?"</question>
  </mapping>

  <mapping>
    <signal>Minimal inline comments, clean code style</signal>
    <question>"The code has very few comments, suggesting a self-documenting code philosophy. Is this intentional? When SHOULD something get a comment?"</question>
  </mapping>

  <mapping>
    <signal>Conventional commits with semantic versioning</signal>
    <question>"Commit history follows conventional commits. Is process discipline a core value, or is it tooling you'd change if something better came along?"</question>
  </mapping>

  <mapping>
    <signal>MIT license with detailed CONTRIBUTING.md</signal>
    <question>"The project is MIT-licensed with a thorough contribution guide. Is openness and community contribution a founding principle, or an operational choice?"</question>
  </mapping>

  <mapping>
    <signal>Monorepo with shared code between services</signal>
    <question>"This is a monorepo with shared code. Is system-wide consistency a principle, or would you split into separate repos if the team grew?"</question>
  </mapping>

</mappings>

---

## Exploration Checklist

<checklist name="codebase-exploration">

  The Explore subagent should gather signals from these sources:

  <item priority="high">README.md — project description and stated purpose</item>
  <item priority="high">AGENTS.md / CLAUDE.md — existing operational philosophy</item>
  <item priority="high">Package manifest (package.json, mix.exs, Cargo.toml, go.mod) — deps and metadata</item>
  <item priority="high">Directory structure (top 2 levels) — organizational philosophy</item>
  <item priority="medium">Test directory structure — testing philosophy</item>
  <item priority="medium">CI config (.github/workflows/, .gitlab-ci.yml) — quality gates</item>
  <item priority="medium">ADRs or decision docs — past trade-off reasoning</item>
  <item priority="medium">LICENSE — openness philosophy</item>
  <item priority="low">Commit message patterns (last 20 commits) — change culture</item>
  <item priority="low">Error handling patterns (sample 3-5 files) — failure philosophy</item>
  <item priority="low">Comment density and style (sample 3-5 files) — documentation culture</item>
  <item priority="low">CONTRIBUTING.md, PR templates — collaboration philosophy</item>

</checklist>

---

## Anti-patterns in Signal Interpretation

<anti-patterns>

  <anti-pattern name="conclusion-jumping">
    <bad>Signal: few dependencies. Conclusion: "This project values self-reliance."</bad>
    <good>Signal: few dependencies. Question: "Is self-reliance intentional, or did it just happen?"</good>
    <reason>The signal might be accidental, historical, or about to change. Always ask.</reason>
  </anti-pattern>

  <anti-pattern name="absence-as-evidence">
    <bad>Signal: no tests. Conclusion: "This project doesn't value quality."</bad>
    <good>Signal: no tests. Question: "Testing isn't prominent yet. Is that a deliberate trade-off for speed, or a gap you want to address?"</good>
    <reason>Absence of something might reflect priorities, not values.</reason>
  </anti-pattern>

  <anti-pattern name="tool-as-philosophy">
    <bad>Signal: uses TypeScript. Conclusion: "Type safety is a core value."</bad>
    <good>Signal: uses TypeScript. Question: "The project uses TypeScript. Is type safety a philosophical commitment, or was it a practical choice for the ecosystem?"</good>
    <reason>Tool choices reflect many factors beyond philosophy.</reason>
  </anti-pattern>

  <anti-pattern name="single-signal-overweight">
    <bad>One file has extensive comments, so "documentation is a core value."</bad>
    <good>Look for patterns across multiple files before forming a hypothesis.</good>
    <reason>Individual files may be anomalous. Look for patterns.</reason>
  </anti-pattern>

</anti-patterns>
