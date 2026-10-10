Answer key (cg graph after the cfg(test) fix, spot-checked against source)

Q1: www/src/broker/api.js createEntity (via postLd) -> antares_api::entities::create_entity
Q2: AnyStore (antares-sql/src/store/any.rs), ExampleStore (examples/plugin-example/src/lib.rs),
    Double (antares-api/tests/common/mod.rs, test), OneDoc (antares-store/src/lib.rs, cfg(test))  [4]
Q3: batch::{batch_create, batch_merge, batch_update, batch_upsert, batch_write},
    entities::{merge_entity, merge_entity_inner}  [7]; antares_api::router optional (registers handlers as fn pointers)
Q4: paging::tests::{count_is_returned_only_on_request, default_page_size_applies, limit_above_the_ceiling_is_too_many_results,
    limit_zero_with_count_yields_an_empty_page_and_the_total, next_and_prev_flag_remaining_elements,
    the_count_only_page_is_the_same_answer_pushed_or_scanned}  [6]
Q5: 32 functions: the 30 from the first key + ExampleStore::{get, temporal_append} (found by the grep agents;
    cg missed them via async_trait spans + static dispatch, fixed); run_current_state_contract accepted either way
Q6: antares-api, antares-broker, antares-plugin-example, antares-sql  [4]
Q7: none. /q/health IS served (antares-api/src/lib.rs:210 "/health" nested under /q); cg's Channel list
    reports it unmatched only because the project link rule assumes the /ngsi-ld/v1 prefix.
Q8: CurrentStateDriver::delete_entity_if, TemporalDriver::delete, CurrentStateDriver::get (via gate! macro body, fixed)  [3];
    TemporalDriver::supported optional
