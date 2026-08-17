'use strict'

// Tests for .github/cla.js.
//
// Run with:  node --test .github/cla.test.js
//
// No test framework, no dependencies — `node:test` and `node:assert` ship with
// the Node versions this project already requires. A CLA check is otherwise
// exercised for the first time on a stranger's pull request, which is a poor
// place to discover a typo.
//
// The tests drive run() through a fake Octokit rather than calling the helper
// functions directly, so what is asserted is what the event handlers actually
// do, not what a unit of them does in isolation.

const test = require('node:test')
const assert = require('node:assert')
const path = require('node:path')

const cla = require(path.join(__dirname, 'cla.js'))

const OWNER = 'gigeey'
const REPO = 'launchpad-studio'
const SIGNATURE_PATH = 'signatures/version1/cla.json'
const DEFAULT_BRANCH = 'main'

const alice = { login: 'alice', id: 1001, type: 'User' }
const bob = { login: 'bob', id: 1002, type: 'User' }
const owner = { login: 'gigeey', id: 258912300, type: 'User' }
const dependabot = { login: 'dependabot[bot]', id: 49699333, type: 'Bot' }

function httpError (status) {
  const err = new Error(`HTTP ${status}`)
  err.status = status
  return err
}

function encode (obj) {
  return Buffer.from(JSON.stringify(obj, null, 2) + '\n', 'utf8').toString('base64')
}

// A fake Octokit that records every write. `signed` is the starting content of
// the signature file (null means the file does not exist yet); `putFailures`
// makes the first N writes fail with the given status, to simulate two people
// signing at the same instant.
function makeGitHub ({ signed = [], commits = [], comments = [], pr = null, getContentError = null, putFailures = 0 } = {}) {
  const calls = { statuses: [], created: [], updated: [], writes: [] }
  let fileSha = signed === null ? undefined : 'filesha0'
  let current = signed
  let remainingPutFailures = putFailures

  const github = {
    calls,
    paginate: async (fn, params) => (await fn(params)).data,
    rest: {
      repos: {
        get: async () => ({ data: { default_branch: DEFAULT_BRANCH } }),
        getContent: async ({ path: p }) => {
          assert.strictEqual(p, SIGNATURE_PATH)
          if (getContentError) throw getContentError
          if (current === null) throw httpError(404)
          return { data: { content: encode({ signedContributors: current }), sha: fileSha } }
        },
        createOrUpdateFileContents: async (params) => {
          if (remainingPutFailures > 0) {
            remainingPutFailures--
            // Somebody else's signature landed first; the sha we hold is stale.
            current = [...(current || []), { name: 'carol', id: 1003 }]
            fileSha = 'filesha1'
            throw httpError(409)
          }
          calls.writes.push(params)
          const decoded = JSON.parse(Buffer.from(params.content, 'base64').toString('utf8'))
          current = decoded.signedContributors
          fileSha = 'filesha2'
          return { data: {} }
        },
        createCommitStatus: async (params) => { calls.statuses.push(params) }
      },
      pulls: {
        get: async () => ({ data: pr }),
        listCommits: async () => ({ data: commits })
      },
      issues: {
        listComments: async () => ({ data: comments }),
        createComment: async (params) => { calls.created.push(params) },
        updateComment: async (params) => { calls.updated.push(params) }
      }
    }
  }
  return github
}

function prContext ({ author = alice, labels = [], headSha = 'head1', number = 7 } = {}) {
  return {
    eventName: 'pull_request_target',
    repo: { owner: OWNER, repo: REPO },
    payload: {
      repository: { id: 555 },
      pull_request: { number, user: author, labels, head: { sha: headSha } }
    }
  }
}

function commentContext ({ user = alice, body = '', number = 7, isPr = true } = {}) {
  return {
    eventName: 'issue_comment',
    repo: { owner: OWNER, repo: REPO },
    payload: {
      repository: { id: 555 },
      issue: { number, pull_request: isPr ? { url: 'x' } : undefined },
      comment: { id: 9001, user, body, created_at: '2026-08-06T10:00:00Z' }
    }
  }
}

const core = { info () {} }

const commitBy = (user, sha) => ({ sha, author: user, commit: { author: { email: 'x@example.com' } } })
const commitUnlinked = (sha) => ({ sha, author: null, commit: { author: { email: 'x@example.com' } } })

const SIGN = 'I have read the CLA Document and I hereby sign the CLA'

// --- pull_request_target ----------------------------------------------------

test('unsigned pull request fails the status and asks the author to sign', async () => {
  const github = makeGitHub({ signed: [], commits: [commitBy(alice, 'c1')] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses.length, 1)
  const status = github.calls.statuses[0]
  assert.strictEqual(status.state, 'failure')
  assert.strictEqual(status.context, 'cla')
  assert.strictEqual(status.sha, 'head1')
  assert.ok(status.description.includes('@alice'))
  assert.ok(status.description.length <= 140)

  assert.strictEqual(github.calls.created.length, 1)
  assert.ok(github.calls.created[0].body.includes('- @alice'))
  assert.ok(github.calls.created[0].body.includes(SIGN))
})

test('signed pull request passes the status', async () => {
  const github = makeGitHub({ signed: [{ name: 'alice', id: 1001 }], commits: [commitBy(alice, 'c1')] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('a signature still counts after the contributor renames their account', async () => {
  const github = makeGitHub({ signed: [{ name: 'alice-old-name', id: 1001 }], commits: [commitBy(alice, 'c1')] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('a commit author who is not the pull request author must sign too', async () => {
  const github = makeGitHub({
    signed: [{ name: 'alice', id: 1001 }],
    commits: [commitBy(alice, 'c1'), commitBy(bob, 'c2')]
  })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'failure')
  assert.ok(github.calls.statuses[0].description.includes('@bob'))
  assert.ok(!github.calls.statuses[0].description.includes('@alice'))
})

test('the project owner and bots are never asked to sign', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(owner, 'c1'), commitBy(dependabot, 'c2')]
  })
  await cla.run({ github, context: prContext({ author: dependabot }), core })

  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('a commit with no GitHub account attached fails the check and is named', async () => {
  const github = makeGitHub({
    signed: [{ name: 'alice', id: 1001 }],
    commits: [commitBy(alice, 'c1'), commitUnlinked('abcdef1234')]
  })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'failure')
  assert.ok(github.calls.statuses[0].description.includes('not linked'))
  assert.ok(github.calls.created[0].body.includes('abcdef1'))
  // The commit author's email must never reach a public comment.
  assert.ok(!github.calls.created[0].body.includes('x@example.com'))
})

test('the waiver label passes the check without reading the signature file', async () => {
  const github = makeGitHub({ signed: null, commits: [] })
  github.rest.repos.getContent = async () => { throw new Error('must not be read when waived') }

  await cla.run({ github, context: prContext({ labels: [{ name: 'cla-not-required' }] }), core })

  assert.strictEqual(github.calls.statuses[0].state, 'success')
  assert.ok(github.calls.statuses[0].description.includes('Waived'))
})

test('a missing signature file means nobody has signed, not a crash', async () => {
  const github = makeGitHub({ signed: null, commits: [commitBy(alice, 'c1')] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'failure')
})

test('an API failure reading signatures is raised, never defaulted to unsigned', async () => {
  const github = makeGitHub({ commits: [commitBy(alice, 'c1')], getContentError: httpError(500) })

  await assert.rejects(() => cla.run({ github, context: prContext(), core }), /HTTP 500/)
  // No status written: a required-but-missing status blocks the merge, which is
  // the fail-closed outcome. A guess in either direction would be a lie.
  assert.strictEqual(github.calls.statuses.length, 0)
})

test('a second run edits the existing comment instead of posting another', async () => {
  const existing = {
    id: 42,
    body: '<!-- cla-check -->\nstale',
    user: { login: 'github-actions[bot]', type: 'Bot' }
  }
  const github = makeGitHub({ signed: [], commits: [commitBy(alice, 'c1')], comments: [existing] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.created.length, 0)
  assert.strictEqual(github.calls.updated.length, 1)
  assert.strictEqual(github.calls.updated[0].comment_id, 42)
})

test('a contributor who quotes the bot does not get their own comment overwritten', async () => {
  // "Quote reply" copies raw source, so the human's comment contains the marker.
  const quotedByHuman = {
    id: 43,
    body: `> <!-- cla-check -->\n> ## Contributor License Agreement\n\nWhich file is the record in?`,
    user: { login: 'alice', type: 'User' }
  }
  const github = makeGitHub({ signed: [], commits: [commitBy(alice, 'c1')], comments: [quotedByHuman] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.updated.length, 0, "must not edit a human's comment")
  assert.strictEqual(github.calls.created.length, 1)
})

test('a stale record without an id still matches by login', async () => {
  const github = makeGitHub({ signed: [{ name: 'alice' }], commits: [commitBy(alice, 'c1')] })
  await cla.run({ github, context: prContext(), core })

  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('a reclaimed login does not inherit the previous owner\'s signature', async () => {
  // alice signed, renamed away, and somebody else took the login.
  const newAlice = { login: 'alice', id: 4242, type: 'User' }
  const github = makeGitHub({ signed: [{ name: 'alice', id: 1001 }], commits: [commitBy(newAlice, 'c1')] })
  await cla.run({ github, context: prContext({ author: newAlice }), core })

  assert.strictEqual(github.calls.statuses[0].state, 'failure')
})

// --- issue_comment ----------------------------------------------------------

test('the signature phrase from a contributor is recorded and turns the status green', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } }
  })
  await cla.run({ github, context: commentContext({ user: alice, body: SIGN }), core })

  assert.strictEqual(github.calls.writes.length, 1)
  const written = JSON.parse(Buffer.from(github.calls.writes[0].content, 'base64').toString('utf8'))
  assert.deepStrictEqual(written.signedContributors, [{
    name: 'alice',
    id: 1001,
    comment_id: 9001,
    created_at: '2026-08-06T10:00:00Z',
    repoId: 555,
    pullRequestNo: 7
  }])
  assert.strictEqual(github.calls.writes[0].branch, DEFAULT_BRANCH)
  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('quoting the reminder comment does not sign the CLA', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } }
  })
  const quoted = `> To sign, add a comment containing exactly:\n>\n>     ${SIGN}\n\nWhat does sublicensable mean here?`
  await cla.run({ github, context: commentContext({ user: alice, body: quoted }), core })

  assert.strictEqual(github.calls.writes.length, 0)
  // Crucially, ordinary conversation must not touch the verdict either way.
  assert.strictEqual(github.calls.statuses.length, 0)
})

test('an ordinary comment leaves the status untouched', async () => {
  const github = makeGitHub({ pr: { number: 7, user: alice, head: { sha: 'head1' } } })
  await cla.run({ github, context: commentContext({ user: alice, body: 'Rebased onto main.' }), core })

  assert.strictEqual(github.calls.statuses.length, 0)
  assert.strictEqual(github.calls.writes.length, 0)
})

test('the phrase is matched case-insensitively and with odd spacing', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } }
  })
  await cla.run({
    github,
    context: commentContext({ user: alice, body: '  i have read the CLA document\n  and I hereby sign the cla  ' }),
    core
  })

  assert.strictEqual(github.calls.writes.length, 1)
})

test('someone with no commits in the pull request cannot sign on it', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } }
  })
  await cla.run({ github, context: commentContext({ user: bob, body: SIGN }), core })

  assert.strictEqual(github.calls.writes.length, 0)
  assert.strictEqual(github.calls.statuses.length, 0)
  assert.strictEqual(github.calls.created.length, 1)
  assert.ok(github.calls.created[0].body.includes('nothing was recorded'))
})

test('a comment on an issue rather than a pull request is ignored', async () => {
  const github = makeGitHub({})
  await cla.run({ github, context: commentContext({ user: alice, body: SIGN, isPr: false }), core })

  assert.strictEqual(github.calls.writes.length, 0)
  assert.strictEqual(github.calls.statuses.length, 0)
})

test('a write that collides with a simultaneous signature is retried, not dropped', async () => {
  const github = makeGitHub({
    signed: [],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } },
    putFailures: 1
  })
  await cla.run({ github, context: commentContext({ user: alice, body: SIGN }), core })

  assert.strictEqual(github.calls.writes.length, 1)
  const written = JSON.parse(Buffer.from(github.calls.writes[0].content, 'base64').toString('utf8'))
  // Both the signature that landed first and this one survive.
  assert.deepStrictEqual(written.signedContributors.map((s) => s.name), ['carol', 'alice'])
  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('signing twice does not append a duplicate record', async () => {
  const github = makeGitHub({
    signed: [{ name: 'alice', id: 1001 }],
    commits: [commitBy(alice, 'c1')],
    pr: { number: 7, user: alice, head: { sha: 'head1' } }
  })
  await cla.run({ github, context: commentContext({ user: alice, body: SIGN }), core })

  assert.strictEqual(github.calls.writes.length, 0)
  assert.strictEqual(github.calls.statuses[0].state, 'success')
})

test('an unhandled event is an error rather than a silent no-op', async () => {
  const github = makeGitHub({})
  const context = { eventName: 'push', repo: { owner: OWNER, repo: REPO }, payload: {} }

  await assert.rejects(() => cla.run({ github, context, core }), /does not handle/)
})
