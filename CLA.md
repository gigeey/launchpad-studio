# Contributor License Agreement

Before your first contribution to Launchpad Studio can be merged, you need to
sign this agreement. It is a one-time step, handled automatically on your first
pull request — see [Signing](#signing) at the bottom.

## Plain English first

We would rather you understand this than skim it, so here is the short version.
It is a summary, not a substitute — the terms below are what actually binds.

- **You keep your copyright.** This is a licence, not an assignment. You do not
  sign your work over to anyone, and you remain free to use your contribution
  anywhere else, for anything, forever.
- **You grant broad rights, including the right to relicense.** The licence you
  grant is sublicensable. In practice this means Gigeey can distribute your
  contribution under licences other than Apache-2.0 — **including proprietary
  or commercial licences** — without asking you again.
- **What this costs you.** You are trusting Gigeey with that discretion. If that
  is not a trade you want to make, that is a legitimate position — please open
  an issue to discuss rather than signing reluctantly. You can also contribute
  without signing by reporting bugs, improving documentation in an issue, or
  discussing design.
- **What we cannot do.** Nothing here lets us take away rights you already have.
  Everything published under Apache-2.0 stays available under Apache-2.0 to
  everyone, permanently, including you.

## 1. Definitions

**"Gigeey"**, **"We"**, or **"Us"** means the entity operating
<https://github.com/gigeey>, and any successor entity to which the Project is
subsequently transferred, together with its successors and assigns.

**"You"** means the copyright owner, or the legal entity authorised by the
copyright owner, entering into this Agreement.

**"Project"** means Launchpad Studio, at
<https://github.com/gigeey/launchpad-studio>.

**"Contribution"** means any original work of authorship, including any
modification to or addition to an existing work, that You intentionally submit
to Us for inclusion in the Project. "Submit" means any form of electronic,
verbal, or written communication sent to Us or our representatives, including
but not limited to pull requests, issues, and discussion on communication
channels managed by Us, excluding communication conspicuously marked or
otherwise designated in writing by You as "Not a Contribution".

## 2. Grant of copyright licence

Subject to the terms of this Agreement, You grant to Us and to recipients of
software distributed by Us a perpetual, worldwide, non-exclusive, no-charge,
royalty-free, irrevocable copyright licence to reproduce, prepare derivative
works of, publicly display, publicly perform, **sublicense**, and distribute
Your Contributions and such derivative works.

For the avoidance of doubt, the sublicensing right granted above permits Us to
distribute Your Contribution under licence terms of Our choosing, including
terms that are not open source.

## 3. Grant of patent licence

Subject to the terms of this Agreement, You grant to Us and to recipients of
software distributed by Us a perpetual, worldwide, non-exclusive, no-charge,
royalty-free, irrevocable (except as stated in this section) patent licence to
make, have made, use, offer to sell, sell, import, and otherwise transfer the
Project, where such licence applies only to those patent claims licensable by
You that are necessarily infringed by Your Contribution alone or by combination
of Your Contribution with the Project.

If any entity institutes patent litigation against You or any other entity
alleging that Your Contribution, or the Project to which You have contributed,
constitutes direct or contributory patent infringement, then any patent licences
granted to that entity under this Agreement for that Contribution or Project
terminate as of the date such litigation is filed.

## 4. Your representations

You represent that:

1. You are legally entitled to grant the above licences.
2. Each of Your Contributions is Your original creation, or You have the right
   to submit it under the terms of this Agreement.
3. If Your employer has rights to intellectual property that You create,
   You have received permission to make the Contributions on behalf of that
   employer, that Your employer has waived such rights, or that Your employer
   has executed a separate agreement with Us.

## 5. Third-party material

Should You wish to submit work that is not Your original creation, You may
submit it separately from any Contribution, identifying the complete details of
its source and of any licence or other restriction of which You are personally
aware, and conspicuously marking the work as "Submitted on behalf of a
third-party: [named here]".

## 6. No obligation and no warranty

You are not expected to provide support for Your Contributions, except to the
extent You desire to provide support. Except for the representations in Section
4, You provide Your Contributions on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied.

We are under no obligation to accept, merge, or use any Contribution.

## 7. Notification

You agree to notify Us of any facts or circumstances of which You become aware
that would make the representations in this Agreement inaccurate in any respect.

## Signing

You do not need to send anything by email or sign a document by hand.

When you open your first pull request, an automated check will post a comment
asking you to sign. Reply to that pull request with exactly:

```
I have read the CLA Document and I hereby sign the CLA
```

Your signature — your GitHub username, the date, and the pull request number —
is recorded in [`signatures/version1/cla.json`](signatures/version1/cla.json) in
this repository, so the record is auditable and stays under your own eyes. You
will not be asked again on later pull requests.

Everyone whose commits are in the pull request signs, not only whoever opened
it. The code that does this is [`.github/cla.js`](.github/cla.js); it is one
file with no dependencies, if you would rather read it than take our word for it.

If the check does not appear on your pull request, please say so in the pull
request rather than assuming it is fine. A CLA check that silently fails to run
is a defect on our side, and we would like to know about it.
