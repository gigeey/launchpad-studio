//! Shared outbound-relay primitives for channels whose bridge threads reply
//! on a "buffer the latest text, flush at `RunEnded`" model — Telegram
//! ([`crate::telegram::outbound`]) and Discord
//! ([`crate::channels::discord::outbound`]) today.
//!
//! [`correlation_map`] is the generic `thread_id -> reply-target` map both
//! channels keep their own copy of
//! ([`crate::telegram::outbound::InFlightChats`],
//! [`crate::channels::discord::InFlightChannels`]). [`observer`] is the
//! generic shape of the `EventBus` subscription loop that drives a relay off
//! it. [`chunker`] is the message-length chunker both channels' outbound
//! sends already use, generalized over a caller-supplied limit. [`lease_gate`]
//! is the single-writer-lease check [`observer`] consults before any
//! outbound send, since both channels' relay observers run process-wide
//! rather than per-binding.
//!
//! Both channels are wired onto this module: `channels::discord::outbound`
//! drives its per-event handling straight through
//! [`observer::handle_relay_event`], and so does `telegram::outbound` —
//! Telegram just wraps that call in a thin outer `EventBus::subscribe()`
//! loop of its own, solely to own the typing-heartbeat lifecycle (start the
//! heartbeat on a relay-eligible `RunStarted`, cancel it at `RunEnded`), a
//! lifecycle concern [`observer`] deliberately doesn't model. A future
//! channel that needs its own status/typing indicator (e.g. setting a
//! thread's status via an external API) doesn't need a private relay core
//! either — wrap [`observer::handle_relay_event`] the way Telegram does.
//!
//! The one duplication that's still genuine: each channel owns its own
//! ~20-line `event_bus.subscribe()` `while let` receive loop rather than
//! sharing one. That's accepted on purpose — it's boilerplate, not where
//! relay bugs live — not an oversight waiting to be "fixed".
//!
//! Per-channel behavior that's genuinely different — wire-format rendering
//! (Telegram's markdown-to-HTML, Discord's near-CommonMark), the send call
//! itself and its result handling, and typing/status indicators — stays
//! where it is and isn't part of this module.
//!
//! [`conversation_gc`] is a separate concern layered on top of
//! [`lease_gate`]: it's the seam a per-channel *inbound* dispatch (rather
//! than the outbound relay this module is otherwise about) calls to run the
//! generic conversation registry's GC pass and release the in-memory lease
//! state of whatever it evicts.

pub(crate) mod chunker;
pub(crate) mod conversation_gc;
pub(crate) mod correlation_map;
pub(crate) mod lease_gate;
pub(crate) mod observer;
