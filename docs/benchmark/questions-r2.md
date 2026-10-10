Repository (read-only, do NOT modify any file): /workspace/codegraph-lab/repo
It is a Rust workspace (crates/*, examples/*) plus a JavaScript/React front end in www/.

Answer these 8 questions about the code. Be precise; list every item, no "etc.".

Q1. Which www front-end function(s) send an HTTP request whose Rust handler reaches the trait method `CurrentStateDriver::delete_entity_if` within 4 call hops? For each: front-end function, route (METHOD path), Rust handler.
Q2. The broker serves an admin surface under `/q`. List every `/q` route (METHOD path) it serves. Which of them does the www front end call, and which Rust function serves that one?
Q3. Which NON-test types implement the trait `ApiSurface`, and what prefix does each return?
Q4. Which NON-test functions call `TemporalDriver::delete`, either on the trait or on a concrete implementation? (module::function)
Q5. How many distinct NON-test functions in crates/antares-api invoke the `gate!` macro?
Q6. Which functions does the body of the `gate!` macro call? (fully qualified)
Q7. If the signature of the trait method `CurrentStateDriver::delete_entity_if` changes, which files OUTSIDE crates/antares-store must be edited (call sites and implementations)?
Q8. List every www/src function that calls the broker's HTTP API, with the route it calls (METHOD path) and the Rust handler that serves it.

Final message format (nothing else):
Q1: ...
...
Q8: ...
CONFIDENCE: one line per question, high/medium/low
