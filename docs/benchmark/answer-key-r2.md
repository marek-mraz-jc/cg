Answer key round 2 (cg graph after link fixes, spot-checked with grep/source)

Q1: www/src/broker/api.js deleteEntity -> DELETE /ngsi-ld/v1/entities/{id} -> antares_api::entities::delete_entity  [1]
    (batchDelete is a trap: batch_delete goes through CurrentStateDriver::batch_delete / delete, not delete_entity_if)
Q2: 9 routes: GET /q/health, GET /q/ready, GET /q/metrics, GET /q/tenants, GET /q/tenants/{tenant}, DELETE /q/tenants/{tenant},
    GET /q/dead-letters, DELETE /q/dead-letters/{id}, POST /q/dead-letters/{id}/replay. www calls only GET /q/health -> antares_api::health
Q3: Admin -> "/q" (crates/antares-api/src/lib.rs); ExampleSurface -> "/x/example" (examples/plugin-example/src/surface.rs)  [2]
Q4: history::mirror_delete_entity, temporal::delete_temporal  [2]  (cg; adjudicate disagreements from source)
Q5: 53  (cg macro REFERENCES; grep enclosing-fn script also 53)
Q6: antares_api::policy::gate, antares_api::snapshots::asking_tenant, antares_api::policy::Operation::new,
    <Filter as Default>::default (derived); engine.as_ref() optional. cg had only the first two (generic path), fixed
Q7: crates/antares-api/src/entities.rs, crates/antares-api/tests/common/mod.rs, crates/antares-sql/src/store/any.rs,
    examples/plugin-example/src/lib.rs  [4]
Q8: 13 (cg LINKS_TO):
    deleteCsr DELETE csourceRegistrations/{} -> csource::delete_registration; deleteEntity DELETE entities/{} -> entities::delete_entity;
    cleanupSpace DELETE subscriptions/{} -> subscriptions::delete_subscription; listCsrs GET csourceRegistrations -> csource::query_registrations;
    listEntities GET entities -> entities::query_entities; listSubscriptions GET subscriptions -> subscriptions::query_subscriptions;
    attrHistory GET temporal/entities/{} -> temporal::retrieve_temporal; health GET /q/health -> antares_api::health;
    registerLink POST csourceRegistrations -> csource::create_registration; createEntity POST entities -> entities::create_entity;
    batchDelete POST entityOperations/delete -> batch::batch_delete; batchUpsert POST entityOperations/upsert -> batch::batch_upsert;
    subscribeAll POST subscriptions -> subscriptions::create_subscription
