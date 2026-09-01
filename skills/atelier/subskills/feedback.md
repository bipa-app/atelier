# Report Atelier feedback

Use this branch when Atelier causes its own friction. The goal is an issue the
maintainers can act on, not a workaround report.

## Classify what happened

- **Suggestion**: a repeated, concrete workflow is harder than it needs to be,
  and the current commands do not already solve it. State the workflow and the
  smaller product change that would remove the friction.
- **Bug**: observed behavior differs from a documented or stable contract,
  including unexpected refusal, panic, wrong output, lost attribution, or a
  state transition that does not match its name.
- **Documentation**: a command, field, example, or state description is wrong
  or stale. Quote the file/URL and the exact claim, then show current behavior.

A user mistake, unavailable dependency, or repo-owned failure is not an
Atelier issue. Trace ownership first.

## Preserve evidence

1. Record `atelier --version`; for a source build, also record its Git SHA.
2. Capture the smallest safe reproduction: starting state, exact commands,
   exact output, expected result, and observed result.
3. Remove access tokens, credentials, signed URLs, bucket names, private
   source content, and home-directory paths. `scripts/collect-diagnostics.sh`
   emits only safe environment facts.
4. Search both open and closed issues:

   ```sh
   gh issue list --repo bipa-app/atelier --state open --search '<keywords>'
   gh issue list --repo bipa-app/atelier --state closed --search '<keywords>'
   ```

5. If an open issue matches, add the new reproduction with
   `gh issue comment <number> --repo bipa-app/atelier --body-file <body>`.
   If no issue matches, open one. If a closed issue still reproduces on the
   latest release, open a new issue and link the closed one.

## Open the issue

Start from `assets/suggestion-issue.md` or `assets/bug-issue.md`, fill every
applicable section, then run:

```sh
scripts/open-feedback-issue.sh suggestion 'Short workflow outcome' /tmp/body.md
scripts/open-feedback-issue.sh bug 'Exact contract that failed' /tmp/body.md
scripts/open-feedback-issue.sh docs 'Wrong command example in integrations guide' /tmp/body.md
```

The script searches titles first and refuses when it finds possible
duplicates. Inspect those issues: comment on a match, or rerun with `--force`
after proving they cover a different defect.

Before finishing the original coding task, include the issue URL in the final
result. That closes the feedback loop without hiding or blocking the user's
requested work.
