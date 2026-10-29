# Locked transport deadline patches

These are local copies of the existing locked crates, not version upgrades.
The Cargo registry is unchanged. Original licenses and package metadata remain in each directory.

| Crate | Archive SHA-256 (also the original Cargo.lock checksum) | Source |
| --- | --- | --- |
| Rocket 0.5.1 | a516907296a31df7dc04310e7043b61d71954d703b603cc6867a026d7e72d73f | https://crates.io/api/v1/crates/rocket/0.5.1/download |
| Hyper 0.14.32 | 41dfc780fdec9373c01bae43289ea34c972e40ee3c9f6b3c8801a35f35586ce7 | https://crates.io/api/v1/crates/hyper/0.14.32/download |

Hyper adds a trusted request extension carrying its existing HTTP/1 header timer deadline.
Rocket adds disabled-by-default timeout configuration and passes that absolute deadline through request-local storage.
For configured POST paths, preprocessing and dispatch share that deadline, including the initial body peek.
Expiration drops the processing future and invokes Rocket's ordinary 504 catcher with Connection: close.
The CLI owns JSON serialization through its registered 504 and 505 catchers.
No timeout wraps the response-body send loop, so existing chat streams retain their lifetime.
The CLI selects 30 seconds and only /v1/systemone. Header parsing is bounded before a path is known.
HTTP/2 SystemOne requests fail with 505; other HTTP/2 routes retain their existing behavior.

Transport deadline changes:

- hyper/src/server/mod.rs
- hyper/src/proto/h1/role.rs
- rocket/src/config/config.rs
- rocket/src/request/request.rs
- rocket/src/server.rs

Local path dependencies also compile under the existing CI `-Dwarnings` flag.
The following compatibility edits register existing build configurations, gate client-only helpers,
replace deprecated integer constants, preserve compile-time thread-safety checks,
and remove unused private members. They do not relax warning levels.

Additional modified files:

- hyper/src/body/body.rs
- hyper/src/body/length.rs
- hyper/src/client/client.rs
- hyper/src/client/conn.rs
- hyper/src/client/connect/http.rs
- hyper/src/error.rs
- hyper/src/ext.rs
- hyper/src/headers.rs
- hyper/src/proto/h1/decode.rs
- hyper/src/proto/h1/io.rs
- hyper/src/proto/mod.rs
- hyper/src/server/conn.rs
- hyper/src/service/make.rs
- rocket/build.rs
- rocket/src/cookies.rs
- rocket/src/form/context.rs
- rocket/src/form/from_form.rs
- rocket/src/form/lenient.rs
- rocket/src/form/strict.rs
- rocket/src/outcome.rs
- rocket/src/shield/policy.rs

The compatibility edits also update hyper/src/proto/h1/role.rs. Original parsing checks and transport limits remain unchanged.

To review against upstream, extract the exact archive into a temporary directory and run diff -ru against its local copy.
Registry-only .cargo-ok extraction markers have no runtime role and are excluded from version control.
