---
name: cashu-security
description: Domain context for security review of Cashu ecash protocol implementations (CDK, nutshell, cashu-ts)
---

<domain>
Cashu is a Chaumian ecash protocol for Bitcoin. Mints issue blind-signed
tokens (proofs) backed by Bitcoin/Lightning deposits. Users hold bearer
tokens that can be transferred, swapped, and redeemed without the mint
learning who spent what.

Core primitives:
- **BDHKE (Blind Diffie-Hellman Key Exchange)** — the blind signature scheme.
  Client blinds a secret, mint signs the blinded value, client unblinds to
  get a valid signature over the secret. The mint never sees the secret.
- **Proofs** — bearer tokens: a tuple of (secret, blinding factor C, mint
  signature). Whoever knows the secret can spend the proof.
- **Keysets** — sets of denomination-specific signing keys. Each keyset has
  a unique ID derived from the public keys. Keysets can be rotated.
- **Blinded messages (B_)** — client-created blinded values sent to the mint
  for signing. The mint returns blinded signatures (C_).

Three main operations:
- **Mint** — deposit sats (via Lightning invoice) → receive proofs
- **Swap** — send proofs as inputs, receive new proofs as outputs (for
  splitting, combining, or refreshing tokens)
- **Melt** — send proofs as inputs → mint pays a Lightning invoice

Spending conditions (NUT-11, NUT-14):
- **P2PK (Pay-to-Public-Key)** — proofs locked to a public key; spending
  requires a valid signature from that key
- **HTLC (Hash Time-Locked Contracts)** — proofs locked to a hash preimage
  with an optional timeout
- **Locktime** — timestamp after which refund keys can spend
- **Refund keys** — alternative spending path activated after locktime
- **n_sigs** — multisig threshold: how many of the listed pubkeys must sign
- **SIG_ALL / SIG_INPUTS** — signature flags controlling which fields the
  signature must cover (all inputs+outputs, or just inputs)

Trust model:
- Clients control: secrets, blinding factors, spending condition construction
- Mint controls: signing keys, proof database (spent/unspent state), fee policy
- Lightning backend: external settlement layer, not directly controlled by
  either party — bugs at this boundary are high-value
</domain>

<invariants>
These are the core security invariants of the Cashu protocol. Frame each
hypothesis as "can I break invariant X via path Y?"

- **Solvency**: sum(outputs) + fees must never exceed sum(inputs) across
  any operation. If this breaks, the attacker mints value from nothing.
- **Single-spend**: a proof can only be redeemed once. Double-spend =
  direct fund extraction.
- **Single-sign**: a blinded message can only be signed once. Re-signing
  the same B_ lets the client unblind multiple valid proofs from one deposit.
- **Issuance integrity**: every spent proof must have been previously issued
  by the mint. Forged proofs = unlimited fund extraction.
- **Spending condition satisfaction**: P2PK/HTLC conditions must be fully
  verified before allowing redemption. Bypass = spending locked tokens
  without authorization.
- **Settlement binding**: Lightning invoices/preimages must be correctly
  bound to quotes. Misbound settlement = paying wrong invoice or extracting
  proofs without valid payment.
</invariants>

<methodology_extensions>
Additional lenses specific to Cashu — use alongside the base methodology:

**Spending condition bypass** — The spending condition system (P2PK, HTLC)
is complex and has been a rich source of bugs:
- Signature verification: does the signed message include all security-relevant
  fields? SIG_ALL must cover outputs, not just inputs.
- Locktime checks: are they compared correctly (<=, <, off-by-one)? What
  happens with locktime=0? What about timezone/format issues?
- Refund key logic: can the refund path be triggered prematurely? What if
  refund keys overlap with primary keys?
- n_sigs threshold: what if n_sigs > len(pubkeys)? What if n_sigs=0?
  What if duplicate pubkeys inflate the apparent key count?
- Tag parsing: duplicate tags (HashMap last-wins vs first-match semantics),
  unknown tag types, missing required tags.

**Settlement boundary** — The boundary between proof DB and external payment
outcomes is where atomicity bugs live:
- Empty/wrong preimage on internal settlement
- Crash reconciliation: what state is left if the process dies mid-operation?
- Invoice↔quote binding: can an attacker substitute invoices?
- Async callback ordering: Lightning callbacks arriving out of order or
  after timeout

**Variant analysis** — When you find a pattern (a check, an assumption, a
fix), search for every other place that pattern should exist. If one swap
path validates keyset IDs but another doesn't, that's the bug. If a race
was fixed on the melt path, check swap and mint for the same race.

**Differential comparison** — If reviewing a shared-spec implementation
(e.g., CDK implementing the same NUTs as nutshell), check whether the other
implementation handles the same edge case differently. Where they disagree,
one is wrong — determine which by reading the NUT spec.

**State and atomicity** — Proof lifecycle under concurrency: row locks,
WAL, saga/reserve patterns. Multi-step operations coordinating DB writes
with settlement — rollback ordering, "publish after commit." Quote
deduplication and blank output uniqueness.
</methodology_extensions>

<severity_context>
Cashu-specific severity guidance:

- **Tier 1 — Loss of funds**: Attacker extracts value never deposited,
  spends proofs without satisfying their conditions, double-spends proofs,
  or permanently destroys other users' ecash.
- **Tier 2 — Preconditions for fund loss**: Crypto verification silently
  accepts malformed input, keyset logic that weakens blinding, state
  corruption chainable with another bug, auth gaps on operator endpoints.
- **Tier 3 — Privacy, availability, information leakage**: Deanonymization
  of token holders, denial-of-service on mint operations, leaking keyset
  rotation schedules.
</severity_context>

<examples>
Cashu-specific hypothesis examples for PR review:

Example 1 — Spending condition bypass:
  "This PR changes P2PK signature verification. Does the SIG_ALL message
  construction include output amounts and input C values? If not, a
  co-signer could redistribute amounts after signing."
  → Trace the message construction end-to-end → verify all fields are covered.

Example 2 — Fee calculation:
  "The swap endpoint accepts inputs and outputs. Fees are calculated from
  input amounts. Does this PR's change allow outputs whose sum exceeds
  inputs minus fees?"
  → Read the sum-checking code → look for off-by-one, type confusion,
  or missing checks on output amounts.

Example 3 — State atomicity:
  "This PR modifies the proof state transition. Can concurrent swap
  requests double-spend by racing between the proof existence check and
  the state update?"
  → Check for row-level locking, reserve patterns, or transaction isolation.

Example 4 — Tag parsing:
  "This PR adds or modifies secret tag parsing. What happens if duplicate
  tags are present? Does the parser use a HashMap (last-wins) while the
  validator assumes first-match semantics?"
  → Send a proof with duplicate locktime tags — one expired, one far-future.

Example 5 — Settlement boundary:
  "This PR changes the melt flow. What happens if the Lightning payment
  succeeds but the process crashes before marking proofs as spent? Can the
  user replay the melt with the same proofs?"
  → Trace the ordering of DB writes vs Lightning calls → check for atomicity.
</examples>
