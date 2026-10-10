Repository (read-only, do NOT modify any file): /workspace/codegraph-lab/repo
It is a Rust workspace (crates/*, examples/*) plus a JavaScript/React front end in www/.

Answer these 8 questions about the code. Be precise; list every item, no "etc.".

Q1. Which function(s) in the www front end send `POST /ngsi-ld/v1/entities`, and which Rust handler function serves that route?
Q2. Which types implement the trait `CurrentStateDriver`? Give type name and file.
Q3. Which NON-test functions reach the function `merge_into` within 3 call hops (direct callers, their callers, and theirs)? Fully qualified names.
Q4. Which test functions exercise `paginate` (call it directly or within 3 hops)?
Q5. Which NON-test functions call the trait method `CurrentStateDriver::get` directly (a `.get(` on a CurrentStateDriver / store value)? List them all (module::function).
Q6. Which workspace crates depend directly on the crate `antares-store` (in their Cargo.toml)?
Q7. Does the www front end call any HTTP path that the Rust server does not serve? Name them, or say none.
Q8. The handler for `DELETE /ngsi-ld/v1/entities/{id}`: which trait methods of antares_store (CurrentStateDriver or TemporalDriver) does it reach within 3 call hops?

Final message format (nothing else):
Q1: ...
...
Q8: ...
CONFIDENCE: one line per question, high/medium/low
