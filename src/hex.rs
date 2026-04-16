// ============================================================
// hex.rs — shared lowercase hex encoding.
//
// Previously duplicated in `api.rs` (JSON response encoding) and
// `db.rs` (TEXT column encoding for `merkle_proofs.root`). A single
// helper prevents silent capitalisation or prefix drift between the
// two sites — a drift that would produce valid-looking but
// non-matching proofs against the on-chain `RewardzRoot.root`.
// ============================================================

/// Encode bytes as a lowercase hex string (no `0x` prefix). Callers
/// add the prefix at the call site when the output format requires
/// it — keeping this helper prefix-free means the same output can
/// be used for both TEXT-column storage (no prefix) and JSON
/// response fields (`0x`-prefixed).
pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
