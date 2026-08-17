# Signatures

`version1/cla.json` is the record of who has agreed to the
[Contributor License Agreement](../CLA.md). It is kept in the repository, rather
than in a service somewhere, so that the record is auditable by the people it
describes — you can read your own entry, and you can watch it being added.

Each entry records a GitHub username, the numeric account id, the comment that
constituted the signature, its timestamp, and the pull request it was given on.
Nothing else. No email addresses.

Entries are appended by [`.github/cla.js`](../.github/cla.js), which runs from
[`.github/workflows/cla.yml`](../.github/workflows/cla.yml). Nobody edits this
file by hand.

## Why the version in the path

If the CLA text ever changes in a way that materially alters what a contributor
agreed to, the honest thing is to ask again rather than carry old signatures
forward against new terms. A versioned path makes that possible without
destroying the existing record: `version2/cla.json` starts empty, and
`version1/cla.json` stays exactly as it is, still showing what each person
actually agreed to and when.

The schema also matches the one used by CLA Assistant Lite, so the record can be
moved to or from that tool without rewriting it.
