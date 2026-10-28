# Walthamstow Agent Instructions

- Walthamstow builds on Clerkenwell and on no product or dataflow framework. A crate never depends
  on a `primrose-*` or `battersea-*` crate; `cargo xtask boundaries` enforces it, and every new
  crate needs a policy entry in `tools/xtask`.
- Durable content lives in a product's workspace paths and journals in the data directory.
  Production code creates no hidden entry but the transient staging `cargo xtask storage-layout`
  lists.
- `docs/contract.md` states what Walthamstow guarantees. A change to a guarantee changes it in
  the same pull request, and a review finding is judged against it.
- Run `cargo xtask check` before finishing. Run Cargo commands one at a time.
- A public API change reviews its snapshot diff and runs `cargo xtask api --update`.
- Tests never sleep, set a timer or read the wall clock, and are never retried. Code under test
  that needs time takes its clock as a dependency.
- A change to a file a workspace holds comes with an explicit upgrade. A journal's entries belong
  to the release that wrote them: a release that changes a journal's format refuses what an
  earlier release left there, and that release finishes it.
- Documentation is en-GB, precise and concise. When something is removed, remove it cleanly,
  without a note that it used to be there.

<!-- newsletter:begin shared · maintained in thirtytwobits/london -->

## Prefer Python over Shell Scripts

Shell scripts suffer from multiple compatibility issues including different shells on
different platforms using different system tools. Python is Python. Prefer using Python
where scripting is needed. 

### Scripting Use Refinement

Only use scripting where another technology does not already provide adequate automation
capabilities. For example, if working in a build system prefer using that build system's
native capabilities first and Python only if those capabilities are not suitable.

# Prose Rules

Use these rules when writing natural language.

## King's English

Use en-GB spelling

## No Stupid Writing Rule

Be Precise and Concise When Writing

### Do not pontificate.

> ❌ The obj backend never emits these attributes: it compiles the C or C++ it generates as part of its own pipeline, and warnings there would be about code the user never sees.

> ✅ The obj backend never emits these attributes.

### Do not prevaricate.

> ❌ There are ways this is implemented and these ways include several which are dangerous and should be used with caution.

> ✅ Do not use raw pointers.

### Do not ramble

Where the banner is self explanatory:

> ❌ Each page carries a gating mode banner. Structural means coverage was inferred from registered test names; behavioural means a cell counts as covered only if a matching test actually executed and passed. The published pages are structural: the docs build compiles the compiler but does not run the test suite. The behavioural run is a release gate in CI, where the generators consume the suite's JUnit results and fail the build on a regression. Read the banner rather than assuming.

> ✅ Each page carries a gating mode banner.

### Do not be inane.

Do not assume the user is an idiot.

> ❌ ... are written by the report generators and rebuilt whenever the site is published. Editing them by hand accomplishes nothing — the next build overwrites the file.

> ✅ ... are written by the report generators and rebuilt whenever the site is published.

## No Kindergarten Headings

> ❌ What is in it

This talks down to an audience that is technical, highly educated, and whom appreciate concise, precise prose.

> ✅ Contents

## No Click-Bait Teasers

> ❌ The one fact that makes this easy
>    You can use cyanoacrylate to tack the workpiece down first.

This is click bait. Do _not_ use anything adjacent to "this one weird trick..." or any other tagline meant to titillate or excite a user into reading something. People will read what they need to read. We are not getting paid to make them navigate through our docs.

> ✅ Use cyanoacrylate to tack the workpiece down first.

Write the fact, not the teaser. In some cases, where something should be emphasised for scanability, use an emoji sigil rather than smarmy text:

> 💡 Tip: Use cyanoacrylate to tack the workpiece down first.

### Uniqueness reveals

> ❌ `LLVM_ENABLE_RTTI=ON` is the one setting that adds rather than removes.
> ❌ The C backend is the only place where a scope crosses a layer.

"The one *X* that …" and "the only *X* that …" are the exemplar above in miniature: the noun
phrase promises something singular and the relative clause delivers it. The reveal reads the same
whether the payoff lands in the same sentence or the next. State the fact; where exclusivity
matters, say it plainly.

> ✅ `LLVM_ENABLE_RTTI=ON` switches a feature on; the other settings here switch features off.
> ✅ Only in the C backend does a scope cross a layer.

`the one` as a pronoun — "the generic name, then the one for this version" — is not this construction.

## No Hyperbole

> ❌ A statically linked binary genuinely has no libc dependency.
> ❌ The runtime read path inherits battle-tested bounds-safety.

An intensifier claims an emphasis the fact does not need, and a marketing adjective claims a quality
the reader cannot check. Delete them; the sentence stands.

> ✅ A statically linked binary has no libc dependency.
> ✅ The runtime read path inherits its bounds-safety.

Intensifiers: *genuinely*, *genuine* as emphasis, *truly*, *really*, *very*, *extremely*,
*incredibly*, *perfectly*, *absolutely*, *utterly*, *hugely*, *vastly*, *remarkably*. Marketing
adjectives: *battle-tested*, *seamless*, *powerful*, *robust*, *sophisticated*, *world-class*, and
*first-class* outside a type system. Keep a word that is part of a term or carries a fact: *deeply
immutable*, *totally ordered*, *this very build*.

## No biting

> ❌ This is where the practice bites.
> ✅ This is where the practice becomes problematic.

"Bites" is a flippant metaphor too often used where the developer should be careful. We should never be
flippant when providing a caution.

## Anaphor rules

In code comments, the use of anaphors must consider the value of paragraphs that are internally complete as readers are often using searches for specific knowledge that does not require the full context of the text above it. Technical writing is not BuzzFeed.

> ❌ There is no affordance for it.

There is no antecedent in the previous sentence. It might be in a title but we probably grepped for this using newlines as a delimiter.

> ✅ There is no affordance for the menu item.

In this version, the real subject survives delineation. Prefer this.

# Documentation Rules

Use these rules when the natural language is documentation of the project, code, or
any other technical instruction.

Prose Rules also apply to documentation where the documentation uses natural language.

## No documentation ghosts (no bananas rule)

When you **remove** something, remove it cleanly — don't leave behind a note that it
used to be there or a negative explaining its absence. The deletion does not need to
be written down.

Turning a removal into a stated negative weakens the logic of the text:

> V1: We want bananas and oranges for the table.
> *(remove bananas)*
> ❌ V2: We want oranges but not bananas for the table.

"Not bananas" is bizarre on its face — it invites the reader to ask *why bananas? why
not apples? Does excluding bananas imply apples are fine?* A clean elision carries
no such baggage:

> ✅ V2: We want oranges for the table.

**The exception — a strong reappearance signal.** Keep the negative only when there
is a concrete, logical force that would otherwise pull the removed thing back: a
common recommendation, an obvious-looking default, something the wider world (or a
model's priors) will keep suggesting. Then the negative is doing real work —
pre-empting the wrong answer — and should carry its *reason*:

> ✅ Use apple juice when sick. We tried orange juice — the usual cold remedy — and it
> did nothing for us.

Here "we tried orange juice" earns its place because the OJ-for-a-cold prior is
strong and would otherwise creep back in. Absent that kind of pull, prefer the
clean elision.

### Oracle variation

Ghosts can also appear as future predictions that have no embodiment. For example:

> ❌ The current UI uses blue and red colours.

"Current" suggests there is a future UI that does not use blue and red colours yet this
plan is not visible and the user is left wondering, "is this UI deprecated? Should I 
be using it? Is blue and red a temporary decision?".

> ✅ The UI uses blue and red colours.

Here we are providing actionable documentation. Yes, the UI is supported. Yes, you
should continue using blue and red colours if you want to be consistent with the
documented GUI.

## CLIs Are Self Documenting

Do not add markdown or other copy-and-paste documentation for CLI flags when `--help` is
adequate and authoritative. There are times when an example of using the CLI to perform
some action is relevant but this is only in service of documenting some other concern
where the CLI can be used, _not_ in documenting the CLI itself.

## Do not break encapsulation to give an example

Do not enumerate actual options as an "example" for something that is meant to be a pass through to a lower domain.

> ❌ // Provide one of the backend implementations -- for example, Rust, C, or C++

This pollutes a higher-level API with knowledge of identifiers that may or may not be valid or remain valid.

> ✅ // Provide one of the backend implementations. See [documentation link] for supported values.

<!-- newsletter:end shared -->
