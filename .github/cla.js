'use strict'

// Contributor License Agreement enforcement.
//
// This file is the whole implementation. It is deliberately dependency-free and
// self-contained: the only third-party code involved is `actions/github-script`,
// which supplies an authenticated Octokit client and nothing else.
//
// WHY THIS IS HAND-WRITTEN RATHER THAN AN OFF-THE-SHELF ACTION
// -----------------------------------------------------------
// The obvious candidate, `contributor-assistant/github-action` (CLA Assistant
// Lite), was archived on 2026-03-23 with "I no longer have the bandwidth to
// maintain this project", and still declares `using: "node20"` — a runtime
// GitHub is retiring. Adopting it would have meant owning a port of 647 commits
// of TypeScript nobody here wrote. The hosted alternative, cla-assistant.io,
// stores signatures in a third party's database, which contradicts the promise
// in CLA.md that the record lives in this repository and stays under the
// contributor's own eyes. So: roughly two hundred lines we can read in one
// sitting, against a first-party action GitHub commits to supporting.
//
// The signature file format is intentionally byte-compatible with CLA Assistant
// Lite's `signedContributors` schema, so the record can be moved to or from that
// tool later without rewriting history.
//
// HOW THE CHECK TURNS GREEN
// -------------------------
// The signal is a *commit status* named `cla`, not the job's own check run. The
// job always exits 0 on an unsigned pull request and records the outcome as a
// status instead. That is what lets a signature posted in a comment flip the
// pull request to green without re-running anything.
//
// Branch protection must therefore require the status context `cla` — not the
// job name. See .github/workflows/cla.yml for the note on this.
//
// If this script throws, no status is written at all, and a required-but-missing
// status blocks the merge. That is deliberate: the failure mode is a stuck pull
// request, never a silently passing one.

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const SIGNATURE_PATH = 'signatures/version1/cla.json'

// The commit status context. Changing this string orphans the branch-protection
// rule that requires it, and every existing pull request goes green.
const STATUS_CONTEXT = 'cla'

// CLA.md tells contributors to reply with exactly this sentence. Stored lower
// case because the comparison is case-insensitive — see matchesSignature().
const SIGNATURE_PHRASE = 'i have read the cla document and i hereby sign the cla'

// Accounts that do not need to sign. The project owner is the party the CLA
// grants rights *to*; a signature from them would be an agreement with
// themselves. Matched on both login and numeric id, because logins can be
// renamed and ids cannot. Bot accounts are excluded separately, by type.
const EXEMPT_LOGINS = ['gigeey']
const EXEMPT_IDS = [258912300]

// A maintainer can apply this label to waive the check — for a bot-authored
// dependency bump, a one-character typo fix, or a contributor whose commits
// cannot be attributed to a GitHub account. The waiver is visible on the pull
// request, so it is a decision on the record rather than a quiet override.
const WAIVER_LABEL = 'cla-not-required'

// Identifies this script's own comment so it can be edited in place rather than
// posted again on every push.
const COMMENT_MARKER = '<!-- cla-check -->'

// GitHub's pull request commits endpoint returns at most this many commits.
// Beyond it the list is truncated, and we say so rather than quietly checking a
// prefix of the contributors and reporting a pass.
const COMMIT_LIST_LIMIT = 250

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function isExempt (user) {
  if (!user) return true
  if (user.type === 'Bot') return true
  if (typeof user.login === 'string' && user.login.endsWith('[bot]')) return true
  if (EXEMPT_IDS.includes(user.id)) return true
  return EXEMPT_LOGINS.some((login) => login.toLowerCase() === String(user.login).toLowerCase())
}

// A comment signs the CLA if, after removing quoted text, it contains the exact
// sentence from CLA.md.
//
// Quoted lines are stripped first because GitHub's "Quote reply" button copies
// the reminder comment — which contains the phrase — into the reply. Without
// this, quoting the bot to ask a question about the CLA would sign it.
//
// Known limitation: a contributor who writes the phrase inside a code block to
// ask about it will also be recorded as having signed. Left as-is rather than
// guessed at, because the alternative is a parser that silently rejects genuine
// signatures, which is the worse failure.
function matchesSignature (body) {
  if (!body) return false
  const unquoted = String(body)
    .split('\n')
    .filter((line) => !/^\s*>/.test(line))
    .join('\n')
  const normalised = unquoted.toLowerCase().replace(/\s+/g, ' ').trim()
  return normalised.includes(SIGNATURE_PHRASE)
}

// Matches on the numeric account id, which is permanent. The login fallback
// exists only for a record written by hand without one, and is deliberately not
// applied to records that do have an id: a login that its owner abandons can be
// claimed by somebody else, and that person would otherwise inherit a signature
// they never gave.
function hasSigned (signed, contributor) {
  return signed.some((record) => {
    if (typeof record.id === 'number') return record.id === contributor.id
    return String(record.name || '').toLowerCase() === String(contributor.login).toLowerCase()
  })
}

async function getDefaultBranch (github, owner, repo) {
  const res = await github.rest.repos.get({ owner, repo })
  return res.data.default_branch
}

// Reads the signature record from a branch. A 404 means the file has not been
// created yet and is the only error treated as "nobody has signed".
//
// Everything else is rethrown on purpose. An API failure that fell through to an
// empty list would report every contributor as unsigned, which looks like a
// working check having a strong opinion; the same defaulting in the other
// direction would pass unsigned pull requests. Neither guess is honest, so the
// job fails and the status is never written.
async function readSignatures (github, owner, repo, ref) {
  try {
    const res = await github.rest.repos.getContent({ owner, repo, path: SIGNATURE_PATH, ref })
    const raw = Buffer.from(res.data.content, 'base64').toString('utf8')
    const parsed = JSON.parse(raw)
    return {
      sha: res.data.sha,
      signed: Array.isArray(parsed.signedContributors) ? parsed.signedContributors : []
    }
  } catch (err) {
    if (err.status === 404) return { sha: undefined, signed: [] }
    throw err
  }
}

// Everyone whose copyright is in this pull request: the author, plus the author
// of every commit.
//
// Checking only the pull request author would make the green check a weaker
// claim than it appears — a pull request opened by one person carrying another
// person's commits would pass with that other person's contribution uncovered.
//
// Known limitation: this reads git commit authorship, so a `Co-Authored-By:`
// trailer is not seen. Someone credited only by trailer will not be asked to
// sign.
async function collectContributors (github, owner, repo, prNumber, prAuthor) {
  const needed = new Map()
  const unlinked = []

  if (!isExempt(prAuthor)) {
    needed.set(prAuthor.id, { login: prAuthor.login, id: prAuthor.id })
  }

  const commits = await github.paginate(github.rest.pulls.listCommits, {
    owner, repo, pull_number: prNumber, per_page: 100
  })

  for (const commit of commits) {
    if (commit.author) {
      if (!isExempt(commit.author)) {
        needed.set(commit.author.id, { login: commit.author.login, id: commit.author.id })
      }
    } else {
      // The commit's author email is not attached to any GitHub account, so
      // there is no account a signature could be matched against. Reported by
      // short SHA only — the email is deliberately not echoed into a public
      // comment.
      unlinked.push(commit.sha.slice(0, 7))
    }
  }

  return {
    needed: [...needed.values()],
    unlinked,
    truncated: commits.length >= COMMIT_LIST_LIMIT
  }
}

async function setStatus (github, owner, repo, sha, state, description, targetUrl) {
  await github.rest.repos.createCommitStatus({
    owner,
    repo,
    sha,
    state,
    context: STATUS_CONTEXT,
    description: description.slice(0, 140),
    target_url: targetUrl
  })
}

async function upsertComment (github, owner, repo, prNumber, body) {
  const comments = await github.paginate(github.rest.issues.listComments, {
    owner, repo, issue_number: prNumber, per_page: 100
  })
  // The author check is load-bearing, not defensive. GitHub's "Quote reply"
  // copies a comment's raw source, HTML comments included, so a contributor who
  // quotes this bot ends up with the marker in their own comment. Matching on
  // the marker alone would then edit that person's words — and the token has
  // permission to do it.
  const existing = comments.find(
    (c) => c.user && c.user.type === 'Bot' && String(c.body || '').includes(COMMENT_MARKER)
  )

  if (existing) {
    if (existing.body === body) return
    await github.rest.issues.updateComment({ owner, repo, comment_id: existing.id, body })
    return
  }
  await github.rest.issues.createComment({ owner, repo, issue_number: prNumber, body })
}

function claUrl (owner, repo, branch) {
  return `https://github.com/${owner}/${repo}/blob/${branch}/CLA.md`
}

function buildReminder ({ outstanding, unlinked, truncated, claLink }) {
  const lines = [COMMENT_MARKER, '', '## Contributor License Agreement', '']

  if (outstanding.length > 0) {
    lines.push(
      'Thanks for the pull request. Before it can be merged, the',
      `[Contributor License Agreement](${claLink}) needs a signature from:`,
      ''
    )
    for (const contributor of outstanding) lines.push(`- @${contributor.login}`)
    lines.push(
      '',
      'To sign, add a comment to this pull request containing exactly:',
      '',
      '    I have read the CLA Document and I hereby sign the CLA',
      '',
      'Please read it first rather than pasting the line. The part most worth',
      'knowing: the licence you grant is **sublicensable**, so your contribution',
      'may be distributed under other licences, including commercial ones. You',
      'keep your own copyright and can use your work anywhere else. If that is',
      'not a trade you want to make, say so in this pull request — that is a',
      'reasonable position and not a problem.',
      '',
      'This is a one-time step. You will not be asked again on later pull requests.'
    )
  }

  if (unlinked.length > 0) {
    lines.push(
      '',
      '### Commits with no GitHub account attached',
      '',
      'These commits were written with an email address that is not linked to any',
      'GitHub account, so there is no account a signature can be matched against:',
      '',
      ...unlinked.map((sha) => `- \`${sha}\``),
      '',
      'Adding that address under Settings → Emails links them retroactively. If',
      'these commits do not need coverage, a maintainer can waive the check with',
      `the \`${WAIVER_LABEL}\` label.`
    )
  }

  if (truncated) {
    lines.push(
      '',
      '> **Note:** this pull request has more than 250 commits, which is the most',
      '> the GitHub API will list. Contributors beyond that point were not',
      '> checked, so treat this result as incomplete rather than as a pass.'
    )
  }

  lines.push(
    '',
    '---',
    '',
    'If this check ever fails to appear on a pull request, please say so rather',
    'than assuming it is fine — a CLA check that silently does not run is a defect',
    'on our side, and we would like to know about it.'
  )

  return lines.join('\n')
}

function buildAllSignedComment (claLink) {
  return [
    COMMENT_MARKER,
    '',
    '## Contributor License Agreement',
    '',
    `Signed — thank you. Every contributor to this pull request has now agreed to the [CLA](${claLink}),`,
    `and the record is in \`${SIGNATURE_PATH}\` in this repository.`,
    '',
    'You will not be asked again on later pull requests.'
  ].join('\n')
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

async function report ({ github, core, owner, repo, prNumber, headSha, defaultBranch, outstanding, unlinked, truncated }) {
  const claLink = claUrl(owner, repo, defaultBranch)
  const satisfied = outstanding.length === 0 && unlinked.length === 0 && !truncated

  if (satisfied) {
    await setStatus(github, owner, repo, headSha, 'success', 'All contributors have signed the CLA.', claLink)
    await upsertComment(github, owner, repo, prNumber, buildAllSignedComment(claLink))
    core.info(`CLA satisfied for #${prNumber} at ${headSha}`)
    return
  }

  const reasons = []
  if (outstanding.length > 0) reasons.push(`awaiting ${outstanding.map((c) => '@' + c.login).join(', ')}`)
  if (unlinked.length > 0) reasons.push(`${unlinked.length} commit(s) not linked to a GitHub account`)
  if (truncated) reasons.push('commit list truncated at 250, result incomplete')

  await setStatus(github, owner, repo, headSha, 'failure', `CLA not satisfied: ${reasons.join('; ')}`, claLink)
  await upsertComment(github, owner, repo, prNumber, buildReminder({ outstanding, unlinked, truncated, claLink }))
  core.info(`CLA not satisfied for #${prNumber} at ${headSha}: ${reasons.join('; ')}`)
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

async function checkPullRequest ({ github, context, core, owner, repo }) {
  const pr = context.payload.pull_request
  const headSha = pr.head.sha
  const defaultBranch = await getDefaultBranch(github, owner, repo)

  if ((pr.labels || []).some((label) => label.name === WAIVER_LABEL)) {
    await setStatus(
      github, owner, repo, headSha, 'success',
      `Waived by a maintainer via the ${WAIVER_LABEL} label.`,
      claUrl(owner, repo, defaultBranch)
    )
    core.info(`CLA waived for #${pr.number} via the ${WAIVER_LABEL} label`)
    return
  }

  const { signed } = await readSignatures(github, owner, repo, defaultBranch)
  const { needed, unlinked, truncated } = await collectContributors(github, owner, repo, pr.number, pr.user)
  const outstanding = needed.filter((contributor) => !hasSigned(signed, contributor))

  await report({
    github, core, owner, repo,
    prNumber: pr.number, headSha, defaultBranch,
    outstanding, unlinked, truncated
  })
}

// Appends one signature to the record on the default branch.
//
// The read and the write are separate API calls, so two people signing within
// the same second can collide: the second write is rejected because the file
// moved. Re-read and retry rather than dropping the signature. The workflow also
// serialises this job through a concurrency group, so the retry is a second line
// of defence, not the only one.
async function appendSignature ({ github, owner, repo, branch, entry }) {
  for (let attempt = 1; attempt <= 3; attempt++) {
    const { sha, signed } = await readSignatures(github, owner, repo, branch)

    if (signed.some((record) => record.id === entry.id)) return signed

    const next = { signedContributors: [...signed, entry] }
    const content = Buffer.from(JSON.stringify(next, null, 2) + '\n', 'utf8').toString('base64')

    try {
      await github.rest.repos.createOrUpdateFileContents({
        owner,
        repo,
        path: SIGNATURE_PATH,
        branch,
        message: `chore(cla): record CLA signature for @${entry.name} (#${entry.pullRequestNo})`,
        content,
        sha
      })
      return next.signedContributors
    } catch (err) {
      const collided = err.status === 409 || err.status === 422
      if (!collided || attempt === 3) throw err
    }
  }
}

async function signFromComment ({ github, context, core, owner, repo }) {
  const comment = context.payload.comment
  const issue = context.payload.issue

  if (!issue.pull_request) return
  if (isExempt(comment.user)) return

  // Ordinary conversation. Return without touching the commit status — a
  // discussion on the pull request must never change the CLA verdict.
  if (!matchesSignature(comment.body)) return

  const defaultBranch = await getDefaultBranch(github, owner, repo)
  const claLink = claUrl(owner, repo, defaultBranch)

  const prRes = await github.rest.pulls.get({ owner, repo, pull_number: issue.number })
  const pr = prRes.data

  const { needed, unlinked, truncated } = await collectContributors(github, owner, repo, pr.number, pr.user)
  const isContributor = needed.some((contributor) => contributor.id === comment.user.id)

  // Only people whose work is actually in this pull request can sign here. A
  // passer-by posting the phrase is not signing anything, and recording it would
  // let any account append commits to the signature file.
  if (!isContributor) {
    await github.rest.issues.createComment({
      owner,
      repo,
      issue_number: pr.number,
      body: [
        `@${comment.user.login} — nothing was recorded, because none of the commits`,
        'in this pull request are yours. The CLA is signed on a pull request that',
        'contains your own work.',
        '',
        `The agreement itself is [here](${claLink}) if you were looking for it.`
      ].join('\n')
    })
    core.info(`Ignored a signature from @${comment.user.login}, who is not a contributor to #${pr.number}`)
    return
  }

  const entry = {
    name: comment.user.login,
    id: comment.user.id,
    comment_id: comment.id,
    created_at: comment.created_at,
    repoId: context.payload.repository.id,
    pullRequestNo: pr.number
  }

  const signed = await appendSignature({ github, owner, repo, branch: defaultBranch, entry })
  core.info(`Recorded a CLA signature for @${entry.name} from #${pr.number}`)

  const outstanding = needed.filter((contributor) => !hasSigned(signed, contributor))

  await report({
    github, core, owner, repo,
    prNumber: pr.number, headSha: pr.head.sha, defaultBranch,
    outstanding, unlinked, truncated
  })
}

// ---------------------------------------------------------------------------

async function run ({ github, context, core }) {
  const { owner, repo } = context.repo

  switch (context.eventName) {
    case 'pull_request_target':
      return checkPullRequest({ github, context, core, owner, repo })
    case 'issue_comment':
      return signFromComment({ github, context, core, owner, repo })
    default:
      throw new Error(`cla.js was invoked for the event "${context.eventName}", which it does not handle.`)
  }
}

module.exports = { run, matchesSignature, hasSigned, isExempt }
