# Constitution Failure Modes

<purpose>
Seven failure modes that ruin project constitutions. Each mode includes detection
signals and prevention strategies. Use this during the Socratic interview to
actively test drafts against these failure modes.
</purpose>

## Contents

- The Motherhood Problem
- The Cookbook Problem
- The Scroll Problem
- The Poster Problem
- The Paralysis Problem
- The Mandate Problem
- The Museum Problem
- Quick Diagnostic

---

## The Motherhood Problem

<failure-mode name="motherhood" severity="critical">

  <description>
    Principles so vague that nobody would disagree with them. "We value quality."
    "We believe in good engineering." These provide zero decision guidance because
    they exclude nothing.
  </description>

  <detection-signals>
    <signal>Would any reasonable person argue AGAINST this principle?</signal>
    <signal>Can you name a specific decision this principle would resolve?</signal>
    <signal>Does the principle say NO to something?</signal>
    <signal>Could you swap it into any other project's constitution unchanged?</signal>
  </detection-signals>

  <prevention>
    <strategy>Apply the Motherhood Test to every principle: "What does this reject?"</strategy>
    <strategy>Ask "If we removed this principle, would any decisions change?"</strategy>
    <strategy>Rewrite as a tension pair: "X over Y" forces specificity</strategy>
  </prevention>

  <examples>
    <example type="bad">"We value code quality" — who doesn't?</example>
    <example type="good">"We value readability over cleverness — obvious code that any team member can debug at 3am beats elegant abstractions that only the author understands"</example>
    <example type="bad">"We believe in testing" — meaningless without a stance</example>
    <example type="good">"We test behaviors, not implementations — if refactoring breaks tests, the tests were wrong"</example>
  </examples>

</failure-mode>

---

## The Cookbook Problem

<failure-mode name="cookbook" severity="high">

  <description>
    Constitution is too specific — it reads like operational documentation or coding
    standards rather than philosophical direction. Prescribes HOW instead of WHY.
    This belongs in AGENTS.md, CONTRIBUTING.md, or linting rules.
  </description>

  <detection-signals>
    <signal>Does the principle mention specific tools, libraries, or versions?</signal>
    <signal>Could this be enforced by a linter or CI rule instead?</signal>
    <signal>Would this become outdated if you changed your tech stack?</signal>
    <signal>Is this an implementation detail masquerading as a value?</signal>
  </detection-signals>

  <prevention>
    <strategy>Ask "Would this survive a complete tech stack migration?"</strategy>
    <strategy>Separate operational rules (AGENTS.md) from philosophical direction (CONSTITUTION.md)</strategy>
    <strategy>If it can be automated, it's not a principle — it's a rule</strategy>
  </prevention>

  <examples>
    <example type="bad">"All functions must be under 20 lines" — this is a linting rule</example>
    <example type="good">"Functions should do one thing and signal their intent through naming" — this is a principle</example>
    <example type="bad">"Use PostgreSQL for all data storage" — this is an architecture decision</example>
    <example type="good">"Choose boring technology for infrastructure — novel tech must justify its novelty" — this is a philosophy</example>
  </examples>

</failure-mode>

---

## The Scroll Problem

<failure-mode name="scroll" severity="high">

  <description>
    Too many principles. Beyond 9, people stop remembering them. A 25-principle
    constitution is a document nobody reads. Miller's Law (7 plus/minus 2) applies.
  </description>

  <detection-signals>
    <signal>More than 9 founding principles</signal>
    <signal>Team members can't name the principles from memory</signal>
    <signal>Principles overlap or could be merged</signal>
    <signal>Some principles are subcases of others</signal>
  </detection-signals>

  <prevention>
    <strategy>Hard cap at 9 founding principles — push back if the user wants more</strategy>
    <strategy>Merge overlapping principles: "simplicity" and "minimalism" are the same value</strategy>
    <strategy>Demote subcases to the Tension Pairs section or Growth Directives</strategy>
    <strategy>Ask "If you could only keep 5, which would they be?"</strategy>
  </prevention>

</failure-mode>

---

## The Poster Problem

<failure-mode name="poster" severity="medium">

  <description>
    Beautiful principles that exist on a wall poster but have zero connection to
    daily practice. Nobody references them in PRs, design discussions, or decisions.
    The constitution exists but doesn't live.
  </description>

  <detection-signals>
    <signal>Can you point to a recent decision where this principle was the tiebreaker?</signal>
    <signal>Would a new team member discover this principle through the codebase, or only through the document?</signal>
    <signal>Is the constitution referenced in PR templates, ADR templates, or design docs?</signal>
  </detection-signals>

  <prevention>
    <strategy>Each principle should include a concrete scenario where it applies</strategy>
    <strategy>Tension Pairs section provides the practical decision framework</strategy>
    <strategy>Suggest integration points: PR templates, ADR preambles, onboarding docs</strategy>
    <strategy>Ask "Where in your workflow would someone actually consult this?"</strategy>
  </prevention>

</failure-mode>

---

## The Paralysis Problem

<failure-mode name="paralysis" severity="high">

  <description>
    Principles conflict with each other and there is no mechanism to resolve the
    conflict. "Move fast" vs. "Be careful" — which wins? Without explicit
    priority or conflict resolution, principles cause analysis paralysis.
  </description>

  <detection-signals>
    <signal>Two principles could argue opposite sides of the same decision</signal>
    <signal>No stated lean or priority ordering</signal>
    <signal>The constitution has never been used to resolve an actual disagreement</signal>
  </detection-signals>

  <prevention>
    <strategy>Tension Pairs section explicitly names conflicts and states the lean</strategy>
    <strategy>Format: "X over Y, but never at the cost of Z"</strategy>
    <strategy>If two principles conflict, ask which wins in the majority of cases</strategy>
    <strategy>The Founding Principles section can have an implicit priority order (first = highest)</strategy>
  </prevention>

</failure-mode>

---

## The Mandate Problem

<failure-mode name="mandate" severity="medium">

  <description>
    Constitution imposed top-down without buy-in from the people who live with it.
    A constitution written by leadership but never discussed with the team creates
    resentment, not alignment.
  </description>

  <detection-signals>
    <signal>Was this written by one person in isolation?</signal>
    <signal>Would the team add or change anything if asked?</signal>
    <signal>Do team members feel ownership of these principles?</signal>
  </detection-signals>

  <prevention>
    <strategy>The Socratic interview process itself prevents this — it requires active user participation</strategy>
    <strategy>Present codebase signals as questions, not conclusions: "The codebase suggests X — is that intentional?"</strategy>
    <strategy>The constitution captures the user's values, not the skill's suggestions</strategy>
    <strategy>Recommend team review after initial drafting</strategy>
  </prevention>

</failure-mode>

---

## The Museum Problem

<failure-mode name="museum" severity="medium">

  <description>
    Constitution fossilized — written once, never updated. The project evolves but
    the constitution doesn't. Old principles become irrelevant or actively harmful,
    but nobody touches them because they feel sacred.
  </description>

  <detection-signals>
    <signal>No amendment history or process</signal>
    <signal>Principles reference concerns that are no longer relevant</signal>
    <signal>Team ignores the constitution because it's outdated</signal>
  </detection-signals>

  <prevention>
    <strategy>Amendments section is mandatory — even if empty at creation</strategy>
    <strategy>Include explicit instructions for how, and by whom, amendment happens</strategy>
    <strategy>Elicit a conscious review decision: a stated trigger, or an explicit "self-initiated only, no trigger" — both pass; only silence fails</strategy>
    <strategy>Where the document sits between living document and foundational stone is the author's ruling — surface the fossilization risk as a question, never impose a cadence</strategy>
  </prevention>

</failure-mode>

---

## Quick Diagnostic

<diagnostic>

  Use this checklist to test a draft constitution against all seven failure modes:

  <check mode="motherhood">For each principle: "Would anyone disagree? What does this reject?"</check>
  <check mode="cookbook">For each principle: "Would this survive a tech stack migration?"</check>
  <check mode="scroll">Count: "Are there 9 or fewer founding principles?"</check>
  <check mode="poster">For each principle: "Name a recent decision this would have resolved."</check>
  <check mode="paralysis">For any pair: "If these conflict, which wins and why?"</check>
  <check mode="mandate">Process: "Was this co-created with the people who live by it?"</check>
  <check mode="museum">Structure: "Is there an amendment process and review trigger?"</check>

</diagnostic>
