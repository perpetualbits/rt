//! Run the xterm/ECMA-48 spec-case table against the in-house `vt_term::Term` — the
//! Phase-3 conformance milestone. The identical table already passes against the
//! vendored oracle (`spec.rs`), so this is the direct measure of the Term's correctness.

use vt_conformance::spec::{cases, run_spec_cases};

#[test]
fn vt_term_passes_the_spec_cases() {
    let fails = run_spec_cases::<vt_term::Term>(&cases());
    assert!(fails.is_empty(), "vt-term spec-case failures ({}):\n{}", fails.len(), fails.join("\n"));
}
