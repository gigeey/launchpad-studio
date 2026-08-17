<!--
  Thanks for the pull request. Everything below is here because it has
  saved a reviewer time before — none of it is ceremony.
-->

## What this changes

<!-- One or two sentences. If it fixes an issue, write "Fixes #123". -->

## Why

<!--
  What problem does this solve? If the reasoning behind an approach is not
  obvious, say what else you considered and why it lost. An unexplained odd
  decision reads as a mistake; the same decision with a sentence of reasoning
  reads as judgement.
-->

## How it was tested

<!--
  Be specific, and please distinguish these two claims, because they are not
  the same thing:
    - the new code works in a test
    - something in the live application actually reaches the new code

  If you added a feature, say what proves a real call path arrives at it.
-->

## Checklist

- [ ] I have read and agree to the [Contributor License Agreement](../CLA.md)
- [ ] `cargo test --workspace --no-fail-fast` passes (on macOS, prefix with
      `LAUNCHPAD_STUDIO_NO_KEYCHAIN=1` — see CONTRIBUTING.md)
- [ ] `npx vitest run` passes in `frontend/`
- [ ] Docs updated in this same pull request if behaviour changed
- [ ] No secrets, API keys, tokens, or personal file paths in the diff
- [ ] Comments describe what the code does, not what it was intended to do

## Anything you are unsure about

<!--
  Genuinely useful. Naming the part you are least confident in gets it
  reviewed properly instead of waved through.
-->
