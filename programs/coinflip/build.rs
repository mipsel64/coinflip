// Shouts at the moment of the mistake: a `local` build is a test artifact, and
// nothing downstream (a stray `anchor deploy`, a copied .so) can tell from the
// binary alone without the byte probes in scripts/verify-artifact.ts.
fn main() {
    if std::env::var("CARGO_FEATURE_LOCAL").is_ok() {
        println!(
            "cargo:warning=coinflip: built with --features local — treasury is the \
             COMMITTED TEST KEY. DO NOT DEPLOY THIS ARTIFACT."
        );
    }
}
