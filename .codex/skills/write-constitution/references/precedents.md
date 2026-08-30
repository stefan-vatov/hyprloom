# Constitution Precedents

<purpose>
Analysis of existing philosophical/principled documents in software. Study what
works, what doesn't, and what patterns to borrow for CONSTITUTION.md.
</purpose>

## Contents

- Zen of Python
- Zen of Go
- The Arch Way
- Agile Manifesto
- Reactive Manifesto
- NHS Constitution
- Synthesis

---

## Zen of Python

<precedent name="zen-of-python" source="PEP 20">

  <format>~19 aphorisms, poetic couplets, standalone statements</format>

  <strengths>
    <strength>Memorable and quotable — developers cite these in code reviews</strength>
    <strength>Many aphorisms implicitly reject something: "Explicit is better than implicit" rejects magic</strength>
    <strength>Tension pairs baked in: "practicality beats purity" acknowledges trade-offs</strength>
    <strength>Short enough to memorize (fits on a t-shirt)</strength>
  </strengths>

  <weaknesses>
    <weakness>Some are vague enough to argue either side ("There should be one obvious way to do it" — but which one?)</weakness>
    <weakness>No enforcement gradient — all aphorisms have equal weight</weakness>
    <weakness>No amendment process — frozen since 2004</weakness>
    <weakness>A few fail the Motherhood Test ("Errors should never pass silently" — who would disagree?)</weakness>
  </weaknesses>

  <lessons>
    <lesson>Poetic/memorable phrasing increases adoption</lesson>
    <lesson>Tension pairs ("Special cases aren't special enough to break the rules" / "Although practicality beats purity") are the most useful entries</lesson>
    <lesson>Keep the count manageable — ~10 logical groupings work</lesson>
  </lessons>

</precedent>

---

## Zen of Go

<precedent name="zen-of-go" source="Dave Cheney, 2020">

  <format>10 proverbs, each with a brief explanation</format>

  <strengths>
    <strength>Each proverb says NO to something specific: "A little copying is better than a little dependency" rejects DRY absolutism</strength>
    <strength>Grounded in real Go idioms — not abstract philosophy</strength>
    <strength>Actionable: developers can apply them in code review</strength>
  </strengths>

  <weaknesses>
    <weakness>Tightly coupled to Go — not transferable to other contexts</weakness>
    <weakness>No explicit trade-off resolution when proverbs conflict</weakness>
  </weaknesses>

  <lessons>
    <lesson>Principles should reject specific alternatives to be meaningful</lesson>
    <lesson>Grounding in real practice (not abstract theory) increases adherence</lesson>
  </lessons>

</precedent>

---

## The Arch Way

<precedent name="arch-way" source="Arch Linux Wiki">

  <format>5 principles with explanations: Simplicity, Modernity, Pragmatism, User Centrality, Versatility</format>

  <strengths>
    <strength>Only 5 principles — easy to remember, hard to ignore</strength>
    <strength>"Simplicity" is defined specifically: "without unnecessary additions or modifications" — rejects auto-configuration</strength>
    <strength>"User centrality" is defined unusually: the USER is expected to understand the system, not be shielded from it</strength>
    <strength>Each principle has a specific, sometimes controversial stance</strength>
  </strengths>

  <weaknesses>
    <weakness>No explicit trade-off resolution between principles</weakness>
    <weakness>Static — no amendment history despite evolving over 20+ years</weakness>
  </weaknesses>

  <lessons>
    <lesson>Fewer principles with specific stances beat many vague ones</lesson>
    <lesson>Defining common words specifically ("simplicity means X, not Y") prevents dilution</lesson>
    <lesson>Controversial stances are features, not bugs — they self-select the community</lesson>
  </lessons>

</precedent>

---

## Agile Manifesto

<precedent name="agile-manifesto" source="agilemanifesto.org, 2001">

  <format>4 value statements ("X over Y") + 12 supporting principles</format>

  <strengths>
    <strength>The "X over Y" format is the most AI-actionable pattern — it provides clear decision defaults</strength>
    <strength>Acknowledges both sides have value: "while there is value in the items on the right, we value the items on the left more"</strength>
    <strength>4 values are instantly memorable; 12 principles provide depth</strength>
    <strength>Signed by specific people — creates accountability</strength>
  </strengths>

  <weaknesses>
    <weakness>The 12 principles mix levels of abstraction (some operational, some philosophical)</weakness>
    <weakness>No update mechanism — frozen since 2001, world has changed</weakness>
    <weakness>"Working software over comprehensive documentation" is cited to justify zero docs (not the intent)</weakness>
    <weakness>Some principles borderline Motherhood: "Build projects around motivated individuals" — who would argue for unmotivated ones?</weakness>
  </weaknesses>

  <lessons>
    <lesson>"X over Y" format is the gold standard for tension pairs</lesson>
    <lesson>Two-level structure (values + principles) provides both memorability and depth</lesson>
    <lesson>MUST include "but never at the cost of Z" to prevent misuse</lesson>
    <lesson>Amendment mechanism is essential — philosophies must evolve</lesson>
  </lessons>

</precedent>

---

## Reactive Manifesto

<precedent name="reactive-manifesto" source="reactivemanifesto.org, 2014">

  <format>4 traits (Responsive, Resilient, Elastic, Message Driven) with architectural explanations</format>

  <strengths>
    <strength>Properties are defined by what they ENABLE, not what they ARE</strength>
    <strength>Explicit dependency graph between traits (Responsive depends on Resilient and Elastic, which depend on Message Driven)</strength>
    <strength>Versioned (v2.0) — acknowledges evolution</strength>
  </strengths>

  <weaknesses>
    <weakness>Too architectural — reads like a spec, not a philosophy</weakness>
    <weakness>Prescriptive about implementation ("message driven") — more Cookbook than Constitution</weakness>
    <weakness>Hard to apply to non-distributed-systems contexts</weakness>
  </weaknesses>

  <lessons>
    <lesson>Showing dependencies between principles helps resolve conflicts</lesson>
    <lesson>Version numbering normalizes evolution</lesson>
    <lesson>Avoid being too prescriptive — constitutions set direction, not implementation</lesson>
  </lessons>

</precedent>

---

## NHS Constitution

<precedent name="nhs-constitution" source="UK National Health Service, 2015">

  <format>Values, Rights, Pledges, Responsibilities — a gradient from hard to soft</format>

  <strengths>
    <strength>Enforcement gradient: Rights (legally enforceable) > Pledges (organizationally committed) > Responsibilities (aspirational)</strength>
    <strength>Different audiences: patients have rights, staff have rights, both have responsibilities</strength>
    <strength>Built-in review cycle (every 10 years, with interim reports)</strength>
    <strength>Preamble grounds everything in purpose ("The NHS belongs to the people")</strength>
  </strengths>

  <weaknesses>
    <weakness>Long — 16 pages, too much for daily reference</weakness>
    <weakness>Bureaucratic tone — hard to feel passionate about</weakness>
  </weaknesses>

  <lessons>
    <lesson>Enforcement gradient maps to software: Hard constraints (CI-enforced) / Principles (review-enforced) / Aspirations (directional)</lesson>
    <lesson>Built-in review cycle prevents fossilization</lesson>
    <lesson>Multi-audience framing can work: developers, reviewers, users</lesson>
  </lessons>

</precedent>

---

## Synthesis

<synthesis>

  <pattern name="optimal-count">
    5-9 founding principles (Miller's 7 plus/minus 2). Beyond this, people stop remembering them.
    The Arch Way (5) and Agile Manifesto (4 values) are at the low end. Zen of Python (~10
    couplets) is at the high end. Both work because they stay within the range.
  </pattern>

  <pattern name="tension-pair-format">
    The Agile Manifesto's "X over Y" format is the most actionable for AI agents.
    Enhance with "but never at the cost of Z" to prevent absolutism.
  </pattern>

  <pattern name="specificity-gradient">
    NHS Constitution's Rights/Pledges/Responsibilities maps to:
    - Boundaries: hard constraints (never cross these)
    - Principles: strong defaults (lean this way)
    - Directives: aspirational vectors (grow toward these)
  </pattern>

  <pattern name="amendment-mechanism">
    Reactive Manifesto versions, NHS review cycles. Without this, constitutions fossilize.
    Include a dated amendment log from day one.
  </pattern>

  <pattern name="memorable-phrasing">
    Zen of Python proves that poetic, quotable phrasing dramatically increases adoption.
    Principles should be short enough to cite in a code review comment.
  </pattern>

  <pattern name="rejection-test">
    Arch Way and Zen of Go show that the best principles explicitly reject something.
    If a principle is universally agreeable, it provides no decision guidance.
  </pattern>

</synthesis>
