# Eclair ↔ LDK Trampoline Interop — Role/Axis Report

_Updated 2026-06-24. Branch `trampoline-interop` / PR carlaKC/ldk-node#1._

## Scope

We test "bare protocol" trampoline interop between **Eclair** (ACINQ
`trampoline-spec-version`, image `carlakirkcohen/eclair:trampoline`) and **LDK**
(ldk-node on the pinned `carlaKC/rust-lightning@2593ba66` fork). Two axes:

- **Unblinded trampoline** — roles: sender, forwarder, receiver.
- **Blinded trampoline** — roles: sender, introduction, relay, receiver.

LDK **intentionally does not** support unblinded send or receive, so along the
unblinded axis only the **forwarder** role is in scope for LDK. Blinded
trampoline additionally requires the **receiver to construct a blinded path
whose hops carry trampoline payloads** (not ordinary forwarding payloads).

Verification is CI-only (the interop stack needs Linux host networking).

## Capability matrix (against shipping APIs today)

| Axis | Role | LDK (ldk-node public API) | Eclair (REST) | Interop testable today |
|------|------|---------------------------|---------------|------------------------|
| Unblinded | sender | ✗ crate-only — descoped by design | ✅ `/payinvoicetrampoline` | ✅ Eclair drives |
| Unblinded | **forwarder** | ✅ forwards (fork force-enables; `PaymentForwarded`) | ✅ auto `NodeRelay` | ✅ **implemented** |
| Unblinded | receiver | ✗ descoped — rejects final HTLC (`IncorrectOrUnknownPaymentDetails`) | ✅ ordinary invoice | Eclair yes; LDK no |
| Blinded | sender | ✗ no custom-route send API | ✅ `/payoffertrampoline` | ❌ nothing valid to pay |
| Blinded | introduction | ✅ auto (experimental, hard-coded) | ❓ ships, no REST trigger | ❌ no path exists |
| Blinded | relay | ✅ auto (experimental) | ❓ ships, no REST trigger | ❌ no path exists |
| Blinded | **receiver** | ✗ `new_for_trampoline` is test-only `pub(crate)` | ✗ offers emit only ordinary blinded paths | ❌ **the gate** |

**The gating finding:** the entire blinded axis is blocked by one missing
capability — **no node, LDK or Eclair, can construct a blinded path carrying
trampoline payloads (the blinded receiver role).** With no such path, the blinded
sender (Eclair is otherwise ready) has nothing to pay, and LDK's working blinded
introduction/relay roles never appear in a real route.

- LDK: `BlindedPaymentPath::new_for_trampoline` exists but is `pub(crate)` +
  `#[cfg(test/_test_utils)]` with a single test-only call site; ldk-node's offer
  receive uses the regular `DefaultRouter::create_blinded_payment_paths`
  (ordinary `ForwardTlvs`).
- Eclair: `OfferCreator`/`DefaultOfferHandler` emit only ordinary forwarding
  blinded paths — no trampoline-payload option, no REST param.

## Implemented tests

Minimal, role-focused: **LDK as unblinded forwarder, success + failure.**

| Test | Topology | Case |
|------|----------|------|
| `test_trampoline_forward` | Eclair-A → **LDK** → Eclair-B | success — asserts LDK `PaymentForwarded` + Eclair `sent` |
| `test_trampoline_forward_unknown_next` | Eclair-A → **LDK** → (no route) | failure — `unknown_next_peer`; Eclair payment fails, LDK emits no `PaymentForwarded` |

### Removed (and why)

- `test_trampoline_receive` (LDK unblinded receiver) — out of scope by design
  (LDK will not add unblinded receive) and proven non-interop.
- `test_trampoline_eclair_forward_unknown_next` (Eclair forwarder failure) — not
  an LDK role; outside the minimal set.
- `open_channel_between_externals` helper — orphaned by the two removals.

## What unlocking LDK blinded-path building would open up

If LDK surfaces the blinded-trampoline-path **constructor** (make
`new_for_trampoline` public/non-test, wire it into
`create_blinded_payment_paths`, add ldk-node offer-builder glue), one
end-to-end **interop** test becomes possible:

> **LDK builds the blinded path (receiver) with Eclair node(s) as the
> introduction and relay hops; Eclair sends via `/payoffertrampoline`.**
> Topology: Eclair-A (sender) → Eclair-T (introduction [→ Eclair relay]) → LDK
> (receiver).

The interop value is confirming **Eclair's introduction and relay roles**
(today "uncertain" — they ship but are untested) against an LDK-built path,
plus Eclair as blinded sender. LDK-as-introduction / LDK-as-relay are
deliberately **not** interop targets — that's LDK↔LDK and belongs in LDK's own
unit tests.

| Blinded role | Who plays it in the interop test | Status after the unlock |
|--------------|----------------------------------|-------------------------|
| **receiver** | LDK (builds the path) | ✅ unlocked |
| sender | Eclair `/payoffertrampoline` | ✅ ready today |
| introduction | **Eclair** | ❓→confirmed by the test |
| relay | **Eclair** | ❓→confirmed by the test |

It does **not** unlock LDK blinded *sender* (needs a separate
`send_payment_with_route` surface) or Eclair blinded *receiver* (Eclair can't
build trampoline-payload paths at all — would need its own constructor).

## Still unsupported / out of scope

- **LDK unblinded sender & receiver** — descoped by design.
- **LDK blinded sender** — no custom-route send API.
- **Eclair blinded receiver** — cannot construct trampoline-payload blinded paths.
- **Sender-selected multi-trampoline chains** — Eclair REST takes one `trampolineNodeId`.
- **Legacy feature-bit (148/149) mismatch** — would need a separate Eclair image.
