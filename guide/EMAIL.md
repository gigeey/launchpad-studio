# Email Channel — Setup & Usage

Connect a Launchpad agent to a real mailbox over IMAP/SMTP so you can email it,
have the agent run against the message, and get a reply back in your inbox —
a full round trip, threaded like a normal conversation.

> **The one thing that trips everyone up:** an agent with an **empty
> allow-list rejects every message** (reject-all security model, same as
> Telegram). You must add at least one address to **Allowed senders** before
> any mail gets through — an unconfigured allow-list silently drops
> everything.

---

## 1. What you need first (Gmail example)

Email connects over IMAP (inbound) and SMTP (outbound), both with **mandatory
TLS** — there is no plaintext option.

1. **Enable 2-Step Verification** on the Google account, if it isn't already.
2. Generate an **App Password** (Google Account → Security → 2-Step
   Verification → App passwords). Google shows it as 16 characters in groups
   of 4 — strip the spaces when you paste it into Launchpad.
3. **IMAP must be enabled** on the mailbox. Older Gmail accounts have an IMAP
   toggle under Settings → Forwarding and POP/IMAP; newer accounts have IMAP
   always-on with no toggle to check.

Any other IMAP/SMTP provider works the same way in principle: TLS-only
connection, an app-specific or account password, and IMAP switched on.

## 2. Connect the mailbox to an agent

In the Launchpad app:

1. Open the agent → **Agent Profile** → **Email** tab.
2. Fill in the connection fields:

   | Field | Example (Gmail) | Notes |
   |---|---|---|
   | Email address | `you@gmail.com` | The mailbox the agent reads and sends from |
   | IMAP host | `imap.gmail.com` | |
   | IMAP port | `993` | Implicit TLS (Launchpad's default) |
   | SMTP host | `smtp.gmail.com` | |
   | SMTP port | `587` | STARTTLS (Launchpad's default). `465` (implicit TLS) also works |
   | Poll interval (seconds) | `300` default — try `15` while testing | How often the inbox is checked for new mail |

3. Enter the **app password** in the separate password field at the bottom of
   the tab (labeled **IMAP/SMTP password**). This field is write-only — once
   saved it shows **Password set** and is never displayed back, so re-enter it
   any time you need to rotate it (**Replace password**).
4. Toggle **Enable Email channel** on.
5. Click **Save configuration**. Saving is what provisions the bridge thread
   that actually receives messages.

Once saved and enabled, the status row at the top of the tab shows
**Enabled**, **Bridge thread provisioned**, and **Password set** — and the
poller begins checking the inbox on your configured interval.

## 3. Allowed senders (the required step)

This is what puts an address on the agent's inbound allow-list. Until you do
this, all mail is dropped — it is the direct email equivalent of Telegram's
"link your chat" step.

1. In the **Email** tab, find **Allowed senders**.
2. Add the bare address(es) allowed to message the agent, e.g. `you@gmail.com`.
   - Matching is case-insensitive and expects a bare address — no display
     name, no surrounding whitespace.
   - You can also add a whole domain with an `@domain.com` entry (e.g.
     `@yourcompany.com`) to allow anyone at that domain.
3. **Save configuration** again to apply the list.

> Leaving **Allowed senders** empty is the single most common setup mistake —
> the channel fails closed and rejects all inbound mail, silently, with
> nothing showing up in the agent's thread.

## 4. Authentication / security settings

**Require authentication results** is on by default. It requires the
receiving mail server's own `Authentication-Results` header to show a DMARC
pass (or aligned SPF/DKIM) before trusting the sender address — a normal
Gmail-to-Gmail send passes this without any extra setup.

While testing end-to-end for the first time, it can help to toggle this
**off** to remove one variable, confirm mail is arriving at all, then turn it
back **on**.

A couple of things worth knowing (informational, not configurable):

- Inbound sender authorization is checked against the **topmost**
  `Authentication-Results` header — the one your own receiving server
  attached — never the spoofable `From:` line.
- Automated and bulk senders are filtered out automatically: addresses like
  `noreply@`/`mailer-daemon@`/`bounce@`, and any message carrying
  `Auto-Submitted`, `Precedence: bulk`, or `List-Unsubscribe` headers.
- Replies use `In-Reply-To`/`References` so threading stays intact in your
  mail client.

## 5. Use it

Send mail to the connected address from an allowed sender. It's ingested into
the agent's bridge thread, the agent runs, and — if it chooses to reply — the
reply goes out through the agent's `SendEmail` action, threaded back to your
original message.

Unlike Telegram, there's no automatic outbound relay of every agent turn:
replying is something the agent explicitly does via `SendEmail`.

---

## Troubleshooting

### No messages reaching the agent

Check **Allowed senders** first — it's almost always this. An empty
allow-list rejects everything. Add the sending address (or an `@domain`
entry), save, and try again.

### App password rejected

Confirm 2-Step Verification is turned on for the Google account (App
Passwords aren't offered without it) and that the password was pasted with
the spaces stripped — Google displays it as `xxxx xxxx xxxx xxxx`, but
Launchpad expects the 16 characters with no spaces.

### Self-signed or local mail servers don't work

TLS certificate verification is enforced with no opt-out. A test/dev IMAP or
SMTP server needs a certificate a normal client would trust — self-signed
certs will fail the connection.

### Mail isn't getting through even though the sender is allowed

Temporarily disable **Require authentication results** to check whether
DMARC/SPF/DKIM alignment is the blocker. If mail starts arriving with it off,
the sending domain's authentication isn't set up the way your receiving
server expects — investigate from there, then turn the setting back on.

---

## Quick reference

| Step | Where | Action |
|---|---|---|
| Get an app password | Google Account → Security | Enable 2-Step Verification → App passwords |
| Connect | App → agent → Email tab | Fill in address/IMAP/SMTP → set password → enable |
| Allow | App → Email tab → Allowed senders | Add sender address(es) or `@domain` |
| Save | App → Email tab | **Save configuration** |
| Use | Your email client | Send mail to the connected address |
| Remove | App → Email tab | **Remove Email channel** |
