//! Integration tests. The network-failure tests need nothing. The rest are `#[ignore]`d because they
//! need a server seeded with `docker/init/*.sql`:
//!
//!   docker compose -f docker/compose.yml up -d pg16
//!   PGB_TEST_URL=postgresql://pgb_admin:admin@localhost:55416/pgb cargo test -p pgcore -- --ignored

use pgcore::catalog;
use pgcore::data::{self, Cell, CellFetch, Filter, Query, Sort};
use pgcore::edit::{self, ApplyError, CellChange, EditSet, NewValue, RowChange};
use pgcore::jsontree::{self, ChildKey, NodeKind, NodeLookup, PathSegment, SetError};
use pgcore::config::{ConnectionParams, parse_conninfo};
use pgcore::error::{ErrorKind, UserFacingError};
use pgcore::session::{Session, SessionState};
use std::time::Duration;

fn params(s: &str) -> ConnectionParams {
    parse_conninfo(s).unwrap().params
}

/// The seeded admin connection. Only called from `#[ignore]`d tests, so a missing variable is a
/// setup error and must fail loudly rather than pass without testing anything.
fn admin() -> ConnectionParams {
    let url = std::env::var("PGB_TEST_URL")
        .expect("PGB_TEST_URL must point at a seeded server, see the comment at the top of this file");
    params(&url)
}

fn as_user(base: &ConnectionParams, user: &str, password: &str) -> ConnectionParams {
    let mut p = base.clone();
    p.user = Some(user.into());
    p.password = Some(password.into());
    p
}

async fn connect_err(p: &ConnectionParams) -> UserFacingError {
    match Session::connect(p).await {
        Ok(_) => panic!("expected the connection to fail"),
        Err(e) => e,
    }
}

// ---- no server required -------------------------------------------------------------------

#[tokio::test]
async fn refused_connection_is_classified() {
    // Port 1 is reserved and nothing listens there.
    let e = connect_err(&params("host=127.0.0.1 port=1 user=x connect_timeout=3")).await;
    assert_eq!(e.kind, ErrorKind::ConnectionRefused, "{e:?}");
    assert!(e.detail.contains("127.0.0.1:1"), "{e:?}");
}

#[tokio::test]
async fn unresolvable_host_is_classified() {
    // `.invalid` is guaranteed never to resolve (RFC 2606).
    let e = connect_err(&params("host=pgb-nope.invalid user=x connect_timeout=5")).await;
    assert_eq!(e.kind, ErrorKind::DnsFailure, "{e:?}");
}

#[tokio::test]
async fn require_ssl_is_refused_before_touching_the_network() {
    let e = connect_err(&params("host=127.0.0.1 port=1 user=x sslmode=require")).await;
    assert_eq!(e.kind, ErrorKind::Unsupported);
}

// ---- server required ----------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn connects_and_reports_server_info() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    assert!(s.is_connected());
    assert!(s.info().version.starts_with("PostgreSQL"));
    assert!(s.info().version_num >= 120000);
    assert_eq!(s.info().database, "pgb");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn lists_seeded_schemas_and_relations() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();

    let schemas = catalog::list_schemas(&s).await.unwrap();
    let shop = schemas.iter().find(|x| x.name == "shop").expect("shop schema");
    assert!(shop.can_use && !shop.is_system);
    assert!(schemas.iter().any(|x| x.name == "information_schema" && x.is_system));
    // Non-system schemas sort before system ones.
    let first_system = schemas.iter().position(|x| x.is_system).unwrap();
    assert!(schemas[..first_system].iter().all(|x| !x.is_system));

    let rels = catalog::list_relations(&s, "shop").await.unwrap();
    let find = |n: &str| rels.iter().find(|r| r.name == n).unwrap_or_else(|| panic!("missing {n}"));
    assert_eq!(find("customers").kind, catalog::RelationKind::Table);
    assert_eq!(find("open_orders").kind, catalog::RelationKind::View);
    assert_eq!(find("daily_orders").kind, catalog::RelationKind::MaterializedView);
    assert_eq!(find("events").kind, catalog::RelationKind::PartitionedTable);
    assert!(find("events_2026").is_partition);
    assert!(find("customers").can_select);
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn wrong_password_is_an_authentication_error() {
    let base = admin();
    // Roles created with a password always require it, so a wrong one must be rejected.
    let e = connect_err(&as_user(&base, "pgb_readonly", "definitely-wrong")).await;
    assert_eq!(e.kind, ErrorKind::Authentication, "{e:?}");
    assert_eq!(e.sqlstate.as_deref(), Some("28P01"));
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn missing_database_is_reported() {
    let mut p = admin();
    p.dbname = Some("pgb_does_not_exist".into());
    let e = connect_err(&p).await;
    assert_eq!(e.kind, ErrorKind::DatabaseNotFound, "{e:?}");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn permission_denied_is_classified_and_navigation_still_lists_the_schema() {
    let base = admin();
    let s = Session::connect(&as_user(&base, "pgb_readonly", "readonly")).await.unwrap();

    // The role has no USAGE on `secret`, but the navigator must still be able to list it (locked).
    let schemas = catalog::list_schemas(&s).await.unwrap();
    let secret = schemas.iter().find(|x| x.name == "secret").expect("secret listed");
    assert!(!secret.can_use);

    let err = s.client().query("SELECT * FROM secret.tokens", &[]).await.unwrap_err();
    let e = UserFacingError::from_pg(&err, Some("test"));
    assert_eq!(e.kind, ErrorKind::Permission, "{e:?}");

    // And it cannot write to shop either.
    let err = s.client().execute("DELETE FROM shop.audit_log", &[]).await.unwrap_err();
    assert_eq!(UserFacingError::from_pg(&err, None).kind, ErrorKind::Permission);
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn syntax_error_reports_position() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    let err = s.client().simple_query("SELECT * FORM shop.customers").await.unwrap_err();
    let e = UserFacingError::from_pg(&err, None);
    assert_eq!(e.kind, ErrorKind::Syntax, "{e:?}");
    assert_eq!(e.position, Some(10), "points at FORM");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn a_running_query_can_be_cancelled() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    let canceller = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await.unwrap();
    });
    let started = std::time::Instant::now();
    let err = s.client().simple_query("SELECT pg_sleep(30)").await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "cancel did not interrupt the query");
    assert_eq!(UserFacingError::from_pg(&err, None).kind, ErrorKind::Cancelled);
    // The session survives a cancel.
    assert!(s.client().simple_query("SELECT 1").await.is_ok());
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn killed_backend_is_reported_as_connection_loss() {
    let p = admin();
    let victim = Session::connect(&p).await.unwrap();
    let killer = Session::connect(&p).await.unwrap();
    let pid: i32 = victim.client().query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    killer.client().execute("SELECT pg_terminate_backend($1)", &[&pid]).await.unwrap();

    let err = victim.client().simple_query("SELECT 1").await.unwrap_err();
    let e = UserFacingError::from_pg(&err, Some("test"));
    assert_eq!(e.kind, ErrorKind::ConnectionLost, "{e:?}");
    assert!(e.retryable);

    let mut state = victim.state();
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(*state.borrow(), SessionState::Closed(_)) {
                return;
            }
            state.changed().await.unwrap();
        }
    })
    .await;
    assert!(closed.is_ok(), "session state never became Closed");
    assert!(!victim.is_connected());
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn pages_through_a_table_with_stable_order_and_preserves_nulls_and_types() {
    let s = Session::connect(&admin()).await.unwrap();

    let cols = data::table_columns(&s, "shop", "customers").await.unwrap();
    let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "email", "full_name", "tags", "profile", "created_at"]);
    assert!(cols[0].is_primary_key && !cols[1].is_primary_key);
    assert_eq!(cols[3].type_name, "text[]");
    assert_eq!(cols[4].type_name, "jsonb");
    assert!(cols[1].not_null && !cols[2].not_null);

    // 2,500 seeded rows: page 1 is full and says there is more, the last page says there is not.
    let first = data::fetch_page(&s, "shop", "customers", &cols, &Query::default(), 0, 500).await.unwrap();
    assert_eq!(first.rows.len(), 500);
    assert!(first.has_more);
    assert_eq!(first.rows[0][0].as_text(), Some("1"), "ordered by primary key");
    assert_eq!(first.rows[0][3].as_text(), Some("{t1}"), "arrays arrive in Postgres text form");
    assert!(first.rows[0][4].as_text().unwrap().starts_with('{'), "jsonb");

    let second = data::fetch_page(&s, "shop", "customers", &cols, &Query::default(), 500, 500).await.unwrap();
    assert_eq!(second.rows[0][0].as_text(), Some("501"), "no overlap between pages");

    let last = data::fetch_page(&s, "shop", "customers", &cols, &Query::default(), 2000, 500).await.unwrap();
    assert_eq!(last.rows.len(), 500);
    assert!(!last.has_more);

    let beyond = data::fetch_page(&s, "shop", "customers", &cols, &Query::default(), 5000, 500).await.unwrap();
    assert!(beyond.rows.is_empty() && !beyond.has_more);
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn distinguishes_null_from_empty_and_renders_bytea_and_ranges() {
    let s = Session::connect(&admin()).await.unwrap();
    let cols = data::table_columns(&s, "shop", "products").await.unwrap();
    let page = data::fetch_page(&s, "shop", "products", &cols, &Query::default(), 0, 10).await.unwrap();
    let idx = |n: &str| cols.iter().position(|c| c.name == n).unwrap();
    let widget = &page.rows[0]; // A-1
    let gadget = &page.rows[1]; // B-2
    assert_eq!(widget[idx("photo")].as_text(), Some("\\xdeadbeef"));
    assert_eq!(widget[idx("dims")].as_text(), Some("[1,10)"));
    assert_eq!(gadget[idx("photo")], Cell::Null, "SQL NULL is None, not an empty string");
    assert_eq!(gadget[idx("dims")], Cell::Null);
    assert_eq!(gadget[idx("price")].as_text(), Some("24.50"), "numeric keeps its scale");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn awkward_identifiers_round_trip_safely() {
    let s = Session::connect(&admin()).await.unwrap();
    let c = s.client();
    c.batch_execute(
        "DROP SCHEMA IF EXISTS pgb_scratch CASCADE; CREATE SCHEMA pgb_scratch; \
         CREATE TABLE pgb_scratch.\"we\"\"ird; DROP TABLE x; --\" (\"col \"\"x\"\"\" int PRIMARY KEY, \"Mixed Case\" text); \
         INSERT INTO pgb_scratch.\"we\"\"ird; DROP TABLE x; --\" VALUES (1, 'a'), (2, NULL);",
    )
    .await
    .unwrap();
    let table = "we\"ird; DROP TABLE x; --";
    let result = async {
        let cols = data::table_columns(&s, "pgb_scratch", table).await?;
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "col \"x\"");
        let page = data::fetch_page(&s, "pgb_scratch", table, &cols, &Query::default(), 0, 10).await?;
        assert_eq!(page.rows, vec![vec![Cell::Text("1".into()), Cell::Text("a".into())], vec![Cell::Text("2".into()), Cell::Null]]);
        Ok::<_, UserFacingError>(())
    }
    .await;
    c.batch_execute("DROP SCHEMA pgb_scratch CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn locked_schemas_locked_tables_and_dropped_tables_report_distinct_errors() {
    let base = admin();
    let ro = Session::connect(&as_user(&base, "pgb_readonly", "readonly")).await.unwrap();

    // No USAGE on the schema: even resolving the table name is refused, and says so.
    let e = data::table_columns(&ro, "secret", "tokens").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Permission, "{e:?}");
    assert!(e.detail.contains("schema secret"), "{e:?}");

    // USAGE on the schema but no SELECT on the table: columns are visible, the read is refused.
    let admin_session = Session::connect(&base).await.unwrap();
    let c = admin_session.client();
    c.batch_execute(
        "DROP SCHEMA IF EXISTS pgb_scratch2 CASCADE; CREATE SCHEMA pgb_scratch2; \
         CREATE TABLE pgb_scratch2.locked (a int PRIMARY KEY); \
         GRANT USAGE ON SCHEMA pgb_scratch2 TO pgb_readonly;",
    )
    .await
    .unwrap();
    let result = async {
        let cols = data::table_columns(&ro, "pgb_scratch2", "locked").await?;
        assert_eq!(cols.len(), 1);
        let e = data::fetch_page(&ro, "pgb_scratch2", "locked", &cols, &Query::default(), 0, 10).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Permission, "{e:?}");
        assert!(e.detail.contains("table locked"), "{e:?}");
        Ok::<_, UserFacingError>(())
    }
    .await;
    c.batch_execute("DROP SCHEMA pgb_scratch2 CASCADE").await.unwrap();
    result.unwrap();

    // A table that does not exist.
    let e = data::table_columns(&admin_session, "shop", "no_such_table").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::ObjectNotFound, "{e:?}");
}

async fn customers(s: &Session) -> Vec<data::Column> {
    data::table_columns(s, "shop", "customers").await.unwrap()
}

fn first_column(page: &data::TablePage) -> Vec<String> {
    page.rows.iter().map(|r| r[0].as_text().unwrap().to_string()).collect()
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn sorting_uses_the_real_type_not_its_text_form_and_pages_stay_consistent() {
    let s = Session::connect(&admin()).await.unwrap();
    let cols = customers(&s).await;
    let desc = Query { sort: Some(Sort { column: "id".into(), descending: true }), filter: Filter::None };
    let p1 = data::fetch_page(&s, "shop", "customers", &cols, &desc, 0, 3).await.unwrap();
    assert_eq!(first_column(&p1), ["2500", "2499", "2498"], "numeric, not text, ordering");
    let p2 = data::fetch_page(&s, "shop", "customers", &cols, &desc, 3, 3).await.unwrap();
    assert_eq!(first_column(&p2), ["2497", "2496", "2495"]);

    // Sorting by a non-unique column is made stable by the primary-key tiebreak.
    let by_tags = Query { sort: Some(Sort { column: "tags".into(), descending: false }), filter: Filter::None };
    let a = data::fetch_page(&s, "shop", "customers", &cols, &by_tags, 0, 50).await.unwrap();
    let b = data::fetch_page(&s, "shop", "customers", &cols, &by_tags, 0, 50).await.unwrap();
    assert_eq!(a.rows, b.rows);

    // json cannot be ordered: that is reported, not swallowed.
    let jsonb = Query { sort: Some(Sort { column: "profile".into(), descending: false }), filter: Filter::None };
    assert!(data::fetch_page(&s, "shop", "customers", &cols, &jsonb, 0, 3).await.is_ok(), "jsonb is orderable");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn raw_filters_count_and_report_errors_at_the_right_place() {
    let s = Session::connect(&admin()).await.unwrap();
    let cols = customers(&s).await;

    let vip = Filter::Raw("(profile->>'vip')::boolean AND id <= 100".into());
    let q = Query { sort: None, filter: vip.clone() };
    let page = data::fetch_page(&s, "shop", "customers", &cols, &q, 0, 500).await.unwrap();
    assert_eq!(page.rows.len(), 10, "ids 10,20,...,100");
    assert!(!page.has_more);
    assert_eq!(data::count_rows(&s, "shop", "customers", &cols, &vip).await.unwrap(), 10);
    assert_eq!(data::count_rows(&s, "shop", "customers", &cols, &Filter::None).await.unwrap(), 2500);

    // A trailing SQL comment in the filter must not break the generated statement.
    let commented = Filter::Raw("id = 5 -- just five".into());
    let page = data::fetch_page(&s, "shop", "customers", &cols, &Query { sort: None, filter: commented }, 0, 10).await.unwrap();
    assert_eq!(first_column(&page), ["5"]);

    // Syntax error: reported as such, with the position relative to what the user typed.
    let bad = Filter::Raw("id > 5 AND AND id < 9".into());
    let e = data::fetch_page(&s, "shop", "customers", &cols, &Query { sort: None, filter: bad.clone() }, 0, 10).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Syntax, "{e:?}");
    // "id > 5 AND " is 11 characters, so the offending second AND starts at character 12.
    assert_eq!(e.position, Some(12), "position is relative to the filter text: {e:?}");
    let e = data::count_rows(&s, "shop", "customers", &cols, &bad).await.unwrap_err();
    assert_eq!(e.position, Some(12));

    // Unknown column in the filter is an ObjectNotFound-class error, position inside the filter.
    let e = data::fetch_page(&s, "shop", "customers", &cols, &Query { sort: None, filter: Filter::Raw("nope = 1".into()) }, 0, 10).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::ObjectNotFound, "{e:?}");
    assert_eq!(e.position, Some(1));

    // The session is still healthy after all of those errors.
    assert!(s.client().simple_query("SELECT 1").await.is_ok());
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn match_filters_follow_keys_including_composite_and_hostile_values() {
    let s = Session::connect(&admin()).await.unwrap();

    let ocols = data::table_columns(&s, "shop", "orders").await.unwrap();
    let m = Filter::Match(vec![("customer_id".into(), "7".into())]);
    let n = data::count_rows(&s, "shop", "orders", &ocols, &m).await.unwrap();
    assert_eq!(n, 2, "orders 7 and 2507 belong to customer 7");
    let page = data::fetch_page(&s, "shop", "orders", &ocols, &Query { sort: None, filter: m }, 0, 10).await.unwrap();
    assert_eq!(page.rows.len(), 2);

    // Composite key: both columns must match.
    let icols = data::table_columns(&s, "shop", "order_items").await.unwrap();
    let both = Filter::Match(vec![("order_id".into(), "1".into()), ("sku".into(), "A-1".into())]);
    assert_eq!(data::count_rows(&s, "shop", "order_items", &icols, &both).await.unwrap(), 1);

    // A hostile value is just data: it matches nothing and breaks nothing.
    let evil = Filter::Match(vec![("sku".into(), "A-1'; DROP TABLE shop.products; --".into())]);
    assert_eq!(data::count_rows(&s, "shop", "order_items", &icols, &evil).await.unwrap(), 0);
    assert!(data::table_columns(&s, "shop", "products").await.is_ok(), "products still exists");

    // A value that cannot be converted to the column type is a clean error, not a crash.
    let bad = Filter::Match(vec![("customer_id".into(), "not-a-number".into())]);
    let e = data::count_rows(&s, "shop", "orders", &ocols, &bad).await.unwrap_err();
    assert_eq!(e.sqlstate.as_deref(), Some("22P02"), "{e:?}");
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn foreign_keys_are_discovered_with_column_order() {
    let s = Session::connect(&admin()).await.unwrap();
    let fks = catalog::foreign_keys(&s, "shop", "orders").await.unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].columns, ["customer_id"]);
    assert_eq!((fks[0].ref_schema.as_str(), fks[0].ref_table.as_str()), ("shop", "customers"));
    assert_eq!(fks[0].ref_columns, ["id"]);

    let items = catalog::foreign_keys(&s, "shop", "order_items").await.unwrap();
    let mut targets: Vec<_> = items.iter().map(|f| (f.ref_table.as_str(), f.columns.clone(), f.ref_columns.clone())).collect();
    targets.sort();
    assert_eq!(targets, [("orders", vec!["order_id".to_string()], vec!["id".to_string()]), ("products", vec!["sku".to_string()], vec!["sku".to_string()])]);

    assert!(catalog::foreign_keys(&s, "shop", "customers").await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn a_slow_count_can_be_cancelled_without_touching_other_connections() {
    let p = admin();
    let counting = Session::connect(&p).await.unwrap();
    let other = Session::connect(&p).await.unwrap();
    let pid: i32 = counting.client().query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);

    let canceller = counting.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await.unwrap();
    });
    // A 4-way cross join of a real catalog table: hours of work if it is not interrupted.
    // (`generate_series` is a poor choice: Postgres does not check for cancellation while it
    // materializes a set-returning function.)
    let slow = counting.client().query_one(
        "SELECT count(*) FROM pg_class a, pg_class b, pg_class c, pg_class d",
        &[],
    );
    let outcome = tokio::time::timeout(Duration::from_secs(15), slow).await;
    if outcome.is_err() {
        // Do not leave a runaway query burning CPU in the test container.
        other.client().execute("SELECT pg_terminate_backend($1)", &[&pid]).await.unwrap();
        panic!("the cancel request did not interrupt the running count within 15s");
    }
    let e = outcome.unwrap().unwrap_err();
    assert_eq!(UserFacingError::from_pg(&e, None).kind, ErrorKind::Cancelled);
    assert!(other.client().simple_query("SELECT 1").await.is_ok(), "the other connection was unaffected");
    assert!(counting.client().simple_query("SELECT 1").await.is_ok(), "the cancelled session is reusable");
}

// ---- large values -----------------------------------------------------------------------------

/// Builds `pgb_large.docs` with one ~30 MB jsonb, a 1 MB text and small neighbours. Dropped again
/// by the caller. Generated server-side so the test transfers almost nothing to create it.
async fn make_large_table(s: &Session) {
    s.client()
        .batch_execute(
            "DROP SCHEMA IF EXISTS pgb_large CASCADE; CREATE SCHEMA pgb_large; \
             CREATE TABLE pgb_large.docs (id int PRIMARY KEY, small jsonb, big jsonb, note text); \
             INSERT INTO pgb_large.docs VALUES \
               (1, '{\"a\": 1}', \
                (SELECT jsonb_build_object('items', jsonb_agg(jsonb_build_object('i', g, 'pad', repeat('x', 120)))) \
                   FROM generate_series(1, 200000) g), \
                repeat('lorem ipsum ', 100000)), \
               (2, NULL, '{\"tiny\": true}', 'short'), \
               (3, '[]', NULL, NULL);",
        )
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn huge_values_never_enter_the_grid_and_load_on_request_under_a_cap() {
    let s = Session::connect(&admin()).await.unwrap();
    make_large_table(&s).await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_large", "docs").await?;

        // 1. The grid page: large cells are represented by their size only.
        let started = std::time::Instant::now();
        let page = data::fetch_page(&s, "pgb_large", "docs", &cols, &Query::default(), 0, 10).await?;
        let page_time = started.elapsed();
        let (r1, r2, r3) = (&page.rows[0], &page.rows[1], &page.rows[2]);
        assert_eq!(r1[1], Cell::Text("{\"a\": 1}".into()), "small jsonb stays inline");
        let Cell::Large { stored_bytes } = r1[2] else { panic!("30 MB jsonb must be Large, got {:?}", r1[2]) };
        // `pg_column_size` is the *stored* size, and Postgres compresses repetitive documents: this
        // ~30 MB document is under 1 MB on disk. The size is real but must be labelled "stored"; the
        // true text size is only known once the value is opened (asserted below via `TooLarge`).
        assert!(stored_bytes > 8_192 && stored_bytes < 5_000_000, "stored size, got {stored_bytes}");
        let Cell::Large { stored_bytes: note_bytes } = r1[3] else { panic!("1.2 MB text must be Large") };
        assert!(note_bytes > 8_192);
        assert_eq!(r2[2], Cell::Text("{\"tiny\": true}".into()));
        assert_eq!(r2[3], Cell::Text("short".into()));
        assert_eq!((r3[2].clone(), r3[3].clone()), (Cell::Null, Cell::Null), "SQL NULL stays distinct from Large");
        let held: usize = page.rows.iter().flatten().filter_map(Cell::as_text).map(str::len).sum();
        assert!(held < 1_000, "the page holds {held} bytes of text; large values must not be resident");

        // 2. The guard really skips the cast: paging must beat serialising the value.
        let started = std::time::Instant::now();
        s.client().query_one("SELECT length(big::text) FROM pgb_large.docs WHERE id = 1", &[]).await?;
        let cast_time = started.elapsed();
        eprintln!("page {page_time:?} vs direct cast of the big value {cast_time:?}");
        if cast_time > Duration::from_millis(30) {
            assert!(page_time * 3 < cast_time, "guarded page ({page_time:?}) should be far faster than the cast ({cast_time:?})");
        }

        // 3. On-demand loading, under a cap.
        let key = vec![("id".to_string(), "1".to_string())];
        let load = |column: &'static str, cap: usize, key: Vec<(String, String)>| {
            let (s, cols) = (&s, &cols);
            async move { data::fetch_cell(s, "pgb_large", "docs", cols, &key, column, cap).await }
        };
        assert_eq!(load("small", 1 << 20, key.clone()).await?, CellFetch::Full("{\"a\": 1}".into()));
        match load("big", 1 << 20, key.clone()).await? {
            CellFetch::TooLarge { total_bytes } => assert!(total_bytes > 20_000_000, "{total_bytes}"),
            other => panic!("a 1 MiB cap must refuse the 30 MB value, got {other:?}"),
        }
        match load("big", 64 << 20, key.clone()).await? {
            CellFetch::Full(text) => {
                assert!(text.len() > 20_000_000);
                assert!(text.starts_with("{\"items\": [{"), "{}", &text[..30]);
            }
            other => panic!("64 MiB cap should load it, got {other:?}"),
        }
        assert_eq!(load("small", 1 << 20, vec![("id".into(), "3".into())]).await?, CellFetch::Full("[]".into()));
        assert_eq!(load("big", 1 << 20, vec![("id".into(), "3".into())]).await?, CellFetch::Null);
        assert_eq!(load("big", 1 << 20, vec![("id".into(), "999".into())]).await?, CellFetch::RowMissing);
        assert!(load("nope", 1 << 20, key).await.is_err(), "unknown column is an error");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_large CASCADE").await.unwrap();
    result.unwrap();
}

// ---- editing --------------------------------------------------------------------------------------

/// A scratch schema with a table covering identity, defaults, NOT NULL, unique, generated, numeric,
/// jsonb and plain json columns. Each test uses its own schema so tests can run in parallel.
async fn make_items(s: &Session, schema: &str) {
    s.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
             CREATE TABLE {schema}.items ( \
               id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \
               name text NOT NULL, qty int NOT NULL DEFAULT 1, price numeric(10,2), \
               meta jsonb, raw json, \
               total numeric GENERATED ALWAYS AS (qty * coalesce(price, 0)) STORED, \
               note text UNIQUE); \
             INSERT INTO {schema}.items (name, qty, price, meta, raw, note) VALUES \
               ('a', 1, 9.99, '{{\"k\": 1}}', '{{\"z\":  2}}', 'n1'), \
               ('b', 2, NULL, NULL, NULL, 'n2'), \
               ('c', 3, 1.00, '[]', '[]', 'n3');"
        ))
        .await
        .unwrap();
}

async fn items(s: &Session, schema: &str) -> (Vec<data::Column>, Vec<Vec<Cell>>) {
    let cols = data::table_columns(s, schema, "items").await.unwrap();
    let page = data::fetch_page(s, schema, "items", &cols, &Query::default(), 0, 100).await.unwrap();
    (cols, page.rows)
}

fn text(v: &str) -> NewValue {
    NewValue::Text(v.to_string())
}

fn set(schema: &str, changes: Vec<RowChange>) -> EditSet {
    EditSet { schema: schema.into(), table: "items".into(), changes }
}

fn key(id: i64) -> Vec<(String, String)> {
    vec![("id".into(), id.to_string())]
}

fn cell_change(cols: &[data::Column], row: &[Cell], column: &str, new: NewValue) -> CellChange {
    let ix = cols.iter().position(|c| c.name == column).unwrap();
    CellChange { column: column.into(), old: row[ix].clone(), new }
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn edits_apply_atomically_and_round_trip_every_kind_of_value() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    make_items(&s, "pgb_edit_roundtrip").await;
    let result = async {
        let (cols, rows) = items(&s, "pgb_edit_roundtrip").await;
        let ix = |n: &str| cols.iter().position(|c| c.name == n).unwrap();
        assert!(cols[ix("total")].generated && !cols[ix("total")].is_writable());
        assert_eq!(cols[ix("id")].identity, data::Identity::ByDefault);

        let evil = "O'Neil\"; DROP TABLE items; --";
        let edit = set("pgb_edit_roundtrip", vec![
            RowChange::Update { key: key(1), changes: vec![
                cell_change(&cols, &rows[0], "name", text(evil)),
                cell_change(&cols, &rows[0], "price", NewValue::Null),
                cell_change(&cols, &rows[0], "meta", text("{\"k\":2,\"new\":[1,2,3]}")),
                cell_change(&cols, &rows[0], "raw", text("{\"z\":   3}")),
                cell_change(&cols, &rows[0], "qty", NewValue::Default),
            ]},
            RowChange::Insert { values: vec![] },                                    // defaults only: fails NOT NULL name...
        ]);
        // ...so first prove a defaults-only insert is rejected by the database, atomically.
        let err = edit::apply(&p, &edit, &cols).await.unwrap_err();
        let ApplyError::Failed { error, change_index } = err else { panic!("expected a database failure, got {err:?}") };
        assert_eq!(error.sqlstate.as_deref(), Some("23502"), "{error:?}");
        assert_eq!(change_index, Some(1));
        let (_, after_failure) = items(&s, "pgb_edit_roundtrip").await;
        assert_eq!(after_failure, rows, "the failed insert must roll back the update before it");

        // Now the real edit set: update + two inserts + a delete, all in one transaction.
        let edit = set("pgb_edit_roundtrip", vec![
            RowChange::Update { key: key(1), changes: vec![
                cell_change(&cols, &rows[0], "name", text(evil)),
                cell_change(&cols, &rows[0], "price", NewValue::Null),
                cell_change(&cols, &rows[0], "meta", text("{\"k\":2,\"new\":[1,2,3]}")),
                cell_change(&cols, &rows[0], "raw", text("{\"z\":   3}")),
                cell_change(&cols, &rows[0], "qty", NewValue::Default),
            ]},
            RowChange::Insert { values: vec![("name".into(), text("only name"))] },
            RowChange::Insert { values: vec![
                ("name".into(), text("full")), ("qty".into(), text("7")), ("price".into(), text("12.50")),
                ("meta".into(), text("{\"a\": null}")), ("note".into(), NewValue::Null),
            ]},
            RowChange::Delete { key: key(3) },
        ]);
        let report = edit::apply(&p, &edit, &cols).await.map_err(UserFacingError::from)?;
        assert_eq!((report.updated, report.inserted, report.deleted), (1, 2, 1));

        let (_, rows) = items(&s, "pgb_edit_roundtrip").await;
        assert_eq!(rows.len(), 4, "3 original - 1 deleted + 2 inserted");
        let r1 = &rows[0];
        assert_eq!(r1[ix("name")], Cell::Text(evil.into()), "quotes and SQL-looking text stored verbatim");
        assert_eq!(r1[ix("price")], Cell::Null);
        assert_eq!(r1[ix("qty")], Cell::Text("1".into()), "DEFAULT applied");
        assert_eq!(r1[ix("meta")], Cell::Text("{\"k\": 2, \"new\": [1, 2, 3]}".into()), "jsonb is normalised by the server");
        assert_eq!(r1[ix("raw")], Cell::Text("{\"z\":   3}".into()), "plain json keeps the exact text");
        assert_eq!(r1[ix("total")], Cell::Text("0".into()), "generated column recomputed");
        assert!(rows.iter().all(|r| r[0] != Cell::Text("3".into())), "row 3 deleted");
        let only = rows.iter().find(|r| r[ix("name")] == Cell::Text("only name".into())).unwrap();
        assert_eq!((only[ix("qty")].clone(), only[ix("note")].clone()), (Cell::Text("1".into()), Cell::Null));
        let full = rows.iter().find(|r| r[ix("name")] == Cell::Text("full".into())).unwrap();
        assert_eq!((full[ix("price")].clone(), full[ix("total")].clone()), (Cell::Text("12.50".into()), Cell::Text("87.50".into())));
        assert!(s.client().query_one("SELECT 1 FROM pgb_edit_roundtrip.items LIMIT 1", &[]).await.is_ok(), "table survived the DROP TABLE text");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_edit_roundtrip CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn a_stale_edit_or_delete_is_a_conflict_and_rolls_back_everything() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    make_items(&s, "pgb_edit_conflict").await;
    let result = async {
        let (cols, rows) = items(&s, "pgb_edit_conflict").await;
        // Someone else changes row 1's name after we loaded it, and deletes row 3.
        s.client().batch_execute("UPDATE pgb_edit_conflict.items SET name = 'changed elsewhere' WHERE id = 1; DELETE FROM pgb_edit_conflict.items WHERE id = 3;").await?;

        let edit = set("pgb_edit_conflict", vec![
            RowChange::Update { key: key(2), changes: vec![cell_change(&cols, &rows[1], "name", text("valid edit"))] },
            RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "name", text("stale edit"))] },
        ]);
        let err = edit::apply(&p, &edit, &cols).await.unwrap_err();
        assert_eq!(err, ApplyError::Conflict { change_index: 1, kind: edit::StatementKind::Update });
        let user: UserFacingError = err.into();
        assert_eq!(user.kind, ErrorKind::Conflict);
        let (_, now) = items(&s, "pgb_edit_conflict").await;
        let name_of = |id: &str| now.iter().find(|r| r[0] == Cell::Text(id.into())).map(|r| r[1].clone());
        assert_eq!(name_of("2"), Some(Cell::Text("b".into())), "the valid edit before the conflict was rolled back");
        assert_eq!(name_of("1"), Some(Cell::Text("changed elsewhere".into())), "the other user's change survives");

        // Deleting a row that is already gone is a conflict too, and rolls back a preceding delete.
        let edit = set("pgb_edit_conflict", vec![RowChange::Delete { key: key(2) }, RowChange::Delete { key: key(3) }]);
        let err = edit::apply(&p, &edit, &cols).await.unwrap_err();
        assert_eq!(err, ApplyError::Conflict { change_index: 1, kind: edit::StatementKind::Delete });
        assert!(items(&s, "pgb_edit_conflict").await.1.iter().any(|r| r[0] == Cell::Text("2".into())), "row 2 not deleted");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_edit_conflict CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn constraint_and_permission_failures_roll_back_and_are_classified() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    make_items(&s, "pgb_edit_fail").await;
    s.client().batch_execute("GRANT USAGE ON SCHEMA pgb_edit_fail TO pgb_readonly; GRANT SELECT ON pgb_edit_fail.items TO pgb_readonly;").await.unwrap();
    let result = async {
        let (cols, rows) = items(&s, "pgb_edit_fail").await;

        // Unique violation on the second change: the first (valid) change must not survive.
        let edit = set("pgb_edit_fail", vec![
            RowChange::Update { key: key(2), changes: vec![cell_change(&cols, &rows[1], "name", text("would be fine"))] },
            RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "note", text("n2"))] },
        ]);
        let ApplyError::Failed { error, change_index } = edit::apply(&p, &edit, &cols).await.unwrap_err() else { panic!() };
        assert_eq!((error.kind, error.sqlstate.as_deref(), change_index), (ErrorKind::Constraint, Some("23505"), Some(1)));
        assert_eq!(items(&s, "pgb_edit_fail").await.1, rows);

        // NOT NULL violation.
        let edit = set("pgb_edit_fail", vec![RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "name", NewValue::Null)] }]);
        let ApplyError::Failed { error, .. } = edit::apply(&p, &edit, &cols).await.unwrap_err() else { panic!() };
        assert_eq!(error.title, "Missing required value");

        // A value that is not valid for the column type: reported, nothing written.
        let edit = set("pgb_edit_fail", vec![RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "qty", text("twelve"))] }]);
        let ApplyError::Failed { error, .. } = edit::apply(&p, &edit, &cols).await.unwrap_err() else { panic!() };
        assert_eq!(error.sqlstate.as_deref(), Some("22P02"), "{error:?}");

        // A read-only role: permission denied, classified, nothing written.
        let mut ro = p.clone();
        ro.user = Some("pgb_readonly".into());
        ro.password = Some("readonly".into());
        let edit = set("pgb_edit_fail", vec![RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "name", text("nope"))] }]);
        let ApplyError::Failed { error, .. } = edit::apply(&ro, &edit, &cols).await.unwrap_err() else { panic!() };
        assert_eq!(error.kind, ErrorKind::Permission, "{error:?}");
        assert_eq!(items(&s, "pgb_edit_fail").await.1, rows, "nothing changed after any failure");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_edit_fail CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn invalid_edits_are_rejected_before_any_connection_is_made() {
    // Points at a port nothing listens on: if apply tried to connect it would report a network error.
    let nowhere = parse_conninfo("host=127.0.0.1 port=1 user=x connect_timeout=2").unwrap().params;
    let mut cols = vec![
        data::Column { name: "id".into(), type_name: "bigint".into(), is_primary_key: true, ..Default::default() },
        data::Column { name: "total".into(), type_name: "numeric".into(), generated: true, ..Default::default() },
    ];
    let edit = set("s", vec![RowChange::Update { key: key(1), changes: vec![CellChange { column: "total".into(), old: Cell::Null, new: text("5") }] }]);
    assert!(matches!(edit::apply(&nowhere, &edit, &cols).await, Err(ApplyError::Invalid(_))), "generated column");
    let edit = set("s", vec![RowChange::Update { key: key(1), changes: vec![CellChange { column: "id".into(), old: Cell::Large { stored_bytes: 9_000_000 }, new: text("5") }] }]);
    assert!(matches!(edit::apply(&nowhere, &edit, &cols).await, Err(ApplyError::Invalid(_))), "large cell");
    cols[0].is_primary_key = false;
    let edit = set("s", vec![RowChange::Delete { key: key(1) }]);
    assert!(matches!(edit::apply(&nowhere, &edit, &cols).await, Err(ApplyError::Invalid(e)) if e.title == "No primary key"));
    // An empty edit set is a no-op that never connects.
    assert_eq!(edit::apply(&nowhere, &set("s", vec![]), &cols).await, Ok(Default::default()));
}

async fn meta_fingerprint(s: &Session) -> String {
    let row = s
        .client()
        .query_one("SELECT md5(meta::text) FROM pgb_edit_huge.items WHERE id = 1", &[])
        .await
        .unwrap();
    row.get(0)
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn editing_a_row_never_touches_its_huge_neighbours_and_a_big_replacement_works() {
    let p = admin();
    let s = Session::connect(&p).await.unwrap();
    make_items(&s, "pgb_edit_huge").await;
    s.client().batch_execute(
        "UPDATE pgb_edit_huge.items SET meta = (SELECT jsonb_build_object('items', jsonb_agg(jsonb_build_object('i', g, 'pad', repeat('x', 120)))) \
                                                 FROM generate_series(1, 200000) g) WHERE id = 1;",
    ).await.unwrap();
    let result = async {
        let (cols, rows) = items(&s, "pgb_edit_huge").await;
        let meta_ix = cols.iter().position(|c| c.name == "meta").unwrap();
        assert!(rows[0][meta_ix].is_large(), "the 30 MB document is not loaded into the grid");
        let before = meta_fingerprint(&s).await;

        // Edit only `name`: the huge `meta` cell is neither sent, compared, nor rewritten.
        let started = std::time::Instant::now();
        let edit = set("pgb_edit_huge", vec![RowChange::Update { key: key(1), changes: vec![cell_change(&cols, &rows[0], "name", text("renamed"))] }]);
        let statements = edit::build(&edit, &cols)?;
        assert!(!statements[0].sql.contains("\"meta\""), "unchanged large columns must not appear in the statement");
        edit::apply(&p, &edit, &cols).await.map_err(UserFacingError::from)?;
        assert!(started.elapsed() < Duration::from_secs(3), "editing beside a 30 MB value took {:?}", started.elapsed());
        assert_eq!(meta_fingerprint(&s).await, before, "the huge value is byte-for-byte unchanged");

        // Replacing a multi-megabyte json document through the normal path works (~2.4 MB).
        let big_doc = format!("{{\"pad\": \"{}\"}}", "lorem ipsum ".repeat(200_000));
        let edit = set("pgb_edit_huge", vec![RowChange::Update { key: key(2), changes: vec![cell_change(&cols, &rows[1], "raw", text(&big_doc))] }]);
        edit::apply(&p, &edit, &cols).await.map_err(UserFacingError::from)?;
        let len: i32 = s.client().query_one("SELECT length(raw::text) FROM pgb_edit_huge.items WHERE id = 2", &[]).await?.get(0);
        assert_eq!(len as usize, big_doc.len(), "the whole document arrived intact");

        // A value too big for a unique index is refused by Postgres with an explanation, and the
        // transaction rolls back cleanly.
        let too_big_for_index = "lorem ipsum ".repeat(200_000);
        let edit = set("pgb_edit_huge", vec![RowChange::Update { key: key(2), changes: vec![cell_change(&cols, &rows[1], "note", text(&too_big_for_index))] }]);
        let ApplyError::Failed { error, .. } = edit::apply(&p, &edit, &cols).await.unwrap_err() else { panic!() };
        assert_eq!(error.title, "Value exceeds a database limit", "{error:?}");
        assert!(error.hint.as_deref().unwrap().contains("index"));
        let note: String = s.client().query_one("SELECT note FROM pgb_edit_huge.items WHERE id = 2", &[]).await?.get(0);
        assert_eq!(note, "n2", "the failed edit changed nothing");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_edit_huge CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn table_access_reports_privileges_and_relation_kind() {
    let base = admin();
    let admin_s = Session::connect(&base).await.unwrap();
    let a = data::table_access(&admin_s, "shop", "customers").await.unwrap();
    assert!(a.is_plain_table() && a.select && a.insert && a.update && a.delete, "{a:?}");
    assert_eq!(data::table_access(&admin_s, "shop", "open_orders").await.unwrap().relkind, "v");
    assert!(!data::table_access(&admin_s, "shop", "daily_orders").await.unwrap().is_plain_table(), "matview");
    assert_eq!(data::table_access(&admin_s, "shop", "events").await.unwrap().relkind, "p");

    let ro = Session::connect(&as_user(&base, "pgb_readonly", "readonly")).await.unwrap();
    let a = data::table_access(&ro, "shop", "customers").await.unwrap();
    assert!(a.select && !a.insert && !a.update && !a.delete, "{a:?}");
}

// ---- json tree: lazy, size-bounded jsonb browsing and node edits --------------------------------

fn k(s: &str) -> PathSegment {
    PathSegment::Key(s.to_string())
}
fn i(n: i64) -> PathSegment {
    PathSegment::Index(n)
}

fn found(lookup: jsontree::NodeLookup) -> jsontree::NodeInfo {
    match lookup {
        NodeLookup::Found(info) => info,
        other => panic!("expected Found, got {other:?}"),
    }
}

async fn make_tree_table(s: &Session, schema: &str) {
    s.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
             CREATE TABLE {schema}.docs (id int PRIMARY KEY, doc jsonb, note text); \
             INSERT INTO {schema}.docs VALUES \
               (1, '{{\"a\": 1, \"b\": {{\"x\": true, \"y\": null}}, \"c\": [10, 20, 30]}}'::jsonb, 'hello'), \
               (2, NULL, 'no doc');"
        ))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn tree_describes_and_lists_children_of_a_small_document() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_small").await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_small", "docs").await?;
        let key1 = key(1);
        let describe = |path: Vec<PathSegment>| {
            let (s, cols) = (&s, &cols);
            let key1 = key1.clone();
            async move { jsontree::describe_node(s, "pgb_tree_small", "docs", cols, &key1, "doc", &path).await }
        };

        let root = found(describe(vec![]).await?);
        assert_eq!((root.kind, root.count), (NodeKind::Object, Some(3)));

        let (children, more) = jsontree::list_children(&s, "pgb_tree_small", "docs", &cols, &key1, "doc", &[], NodeKind::Object, 0, 10).await?;
        assert!(!more);
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].key, ChildKey::Key("a".into()));
        assert_eq!(children[0].info.preview.as_deref(), Some("1"));
        assert_eq!(children[1].key, ChildKey::Key("b".into()));
        assert_eq!(children[1].info.kind, NodeKind::Object);
        assert_eq!(children[1].info.preview, None, "containers have no preview");

        let b = found(describe(vec![k("b")]).await?);
        assert_eq!((b.kind, b.count), (NodeKind::Object, Some(2)));
        let (bkids, _) = jsontree::list_children(&s, "pgb_tree_small", "docs", &cols, &key1, "doc", &[k("b")], NodeKind::Object, 0, 10).await?;
        assert_eq!(bkids[0].info.preview.as_deref(), Some("true"));
        assert_eq!(bkids[1].info.kind, NodeKind::Null);
        assert_eq!(bkids[1].info.preview.as_deref(), Some("null"));

        let c = found(describe(vec![k("c")]).await?);
        assert_eq!((c.kind, c.count), (NodeKind::Array, Some(3)));
        let (ckids, _) = jsontree::list_children(&s, "pgb_tree_small", "docs", &cols, &key1, "doc", &[k("c")], NodeKind::Array, 0, 10).await?;
        assert_eq!(ckids.iter().map(|c| c.key.clone()).collect::<Vec<_>>(), vec![ChildKey::Index(0), ChildKey::Index(1), ChildKey::Index(2)]);
        assert_eq!(ckids[1].info.preview.as_deref(), Some("20"));

        // A path that does not exist, and a row that does not exist.
        assert_eq!(describe(vec![k("nope")]).await?, NodeLookup::Missing);
        assert_eq!(
            jsontree::describe_node(&s, "pgb_tree_small", "docs", &cols, &key(2), "doc", &[]).await?,
            NodeLookup::Missing,
            "column is SQL NULL"
        );
        assert_eq!(
            jsontree::describe_node(&s, "pgb_tree_small", "docs", &cols, &key(999), "doc", &[]).await?,
            NodeLookup::RowMissing
        );
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_small CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn tree_never_fetches_more_than_the_targeted_bytes_of_a_huge_document() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_huge").await;
    let cols = data::table_columns(&s, "pgb_tree_huge", "docs").await.unwrap();
    let k1 = key(1);

    // A meaningful "is this bounded?" check has to be robust to this machine's fixed per-query
    // overhead (parse/plan/round-trip), which can itself be tens of milliseconds and would make any
    // absolute or full-cast-relative timing assertion flaky. So the real comparison is *this
    // operation on the small seed document* vs *the same operation once the document is ~30 MB*:
    // describing/listing must not get much slower, even though a full cast of the document does.
    let t0 = std::time::Instant::now();
    let small_root = jsontree::describe_node(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[]).await;
    let small_time = t0.elapsed();
    assert_eq!(found(small_root.unwrap()).count, Some(3), "the seed doc has 3 top-level keys (a, b, c)");

    let result = async {
        s.client()
            .batch_execute(
                "UPDATE pgb_tree_huge.docs SET doc = \
                 (SELECT jsonb_build_object('items', jsonb_agg(jsonb_build_object('i', g, 'pad', repeat('x', 120)))) \
                    FROM generate_series(1, 200000) g) WHERE id = 1;",
            )
            .await?;

        let t0 = std::time::Instant::now();
        let huge_root = jsontree::describe_node(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[]).await;
        let huge_time = t0.elapsed();
        let root = found(huge_root?);
        assert_eq!((root.kind, root.count), (NodeKind::Object, Some(1)));

        let items = found(jsontree::describe_node(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[k("items")]).await?);
        assert_eq!((items.kind, items.count), (NodeKind::Array, Some(200_000)));

        let t0 = std::time::Instant::now();
        let (first5, more) =
            jsontree::list_children(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[k("items")], NodeKind::Array, 0, 5).await?;
        let list_time = t0.elapsed();
        assert!(more);
        assert_eq!(first5.len(), 5);
        assert_eq!(first5[0].key, ChildKey::Index(0));
        assert_eq!(first5[0].info.kind, NodeKind::Object, "each element is {{i, pad}}");

        // A late offset into a huge array is inherently O(offset): jsonb_array_elements has no
        // random-access seek, so reaching the end costs roughly what reading the whole array does.
        // That is an accepted trade-off (see the comment on `list_children`); just check it
        // terminates promptly, not that it is fast.
        let t0 = std::time::Instant::now();
        let (last, more) =
            jsontree::list_children(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[k("items")], NodeKind::Array, 199_998, 5).await?;
        assert!(t0.elapsed() < Duration::from_secs(10), "even the worst case must terminate promptly: {:?}", t0.elapsed());
        assert!(!more);
        assert_eq!(last.len(), 2);
        assert_eq!(last[1].key, ChildKey::Index(199_999));

        // Drill into one element: a small object with a small string leaf.
        let elem0 = found(jsontree::describe_node(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[k("items"), i(0)]).await?);
        assert_eq!((elem0.kind, elem0.count), (NodeKind::Object, Some(2)));
        let (fields, _) = jsontree::list_children(&s, "pgb_tree_huge", "docs", &cols, &k1, "doc", &[k("items"), i(0)], NodeKind::Object, 0, 10).await?;
        assert_eq!(fields[0].info.preview.as_deref(), Some("1"));
        assert_eq!(fields[1].info.preview, Some(format!("\"{}\"", "x".repeat(120))));

        let t0 = std::time::Instant::now();
        s.client().query_one("SELECT length(doc::text) FROM pgb_tree_huge.docs WHERE id = 1", &[]).await?;
        let full_cast_time = t0.elapsed();
        eprintln!(
            "describe: {small_time:?} (small doc) -> {huge_time:?} (30 MB doc); list first page {list_time:?}; full cast {full_cast_time:?}"
        );
        // Growing the document ~1000x must not make `describe_node` dramatically slower: generous
        // (5x + 200ms) so ordinary scheduling jitter on a shared CI machine can't make this flaky.
        assert!(
            huge_time < small_time * 10 + Duration::from_millis(750),
            "describe_node scaled with document size: {small_time:?} -> {huge_time:?}"
        );
        if full_cast_time > Duration::from_millis(300) {
            assert!(list_time * 3 < full_cast_time, "{list_time:?} vs {full_cast_time:?}");
        }
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_huge CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn editing_a_leaf_touches_only_that_path_in_a_huge_document() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_edit").await;
    s.client()
        .batch_execute(
            "UPDATE pgb_tree_edit.docs SET doc = \
             (SELECT jsonb_build_object('items', jsonb_agg(jsonb_build_object('i', g, 'pad', repeat('x', 120)))) \
                FROM generate_series(1, 200000) g) WHERE id = 1;",
        )
        .await
        .unwrap();
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_edit", "docs").await?;
        let k1 = key(1);
        let xmin = jsontree::read_xmin(&s, "pgb_tree_edit", "docs", &cols, &k1).await?.unwrap();

        let checksum = || {
            let s = &s;
            async move {
                let row = s.client().query_one("SELECT md5(doc::text) FROM pgb_tree_edit.docs WHERE id = 1", &[]).await.unwrap();
                row.get::<_, String>(0)
            }
        };
        let before = checksum().await;

        let t0 = std::time::Instant::now();
        jsontree::set_at_path(&s, "pgb_tree_edit", "docs", &cols, &k1, "doc", &[k("items"), i(0), k("i")], "999", &xmin)
            .await
            .map_err(UserFacingError::from)?;
        assert!(t0.elapsed() < Duration::from_secs(3), "editing beside a 30 MB document took {:?}", t0.elapsed());
        assert_ne!(checksum().await, before, "the document did change");

        let edited = found(jsontree::describe_node(&s, "pgb_tree_edit", "docs", &cols, &k1, "doc", &[k("items"), i(0), k("i")]).await?);
        assert_eq!(edited.preview.as_deref(), Some("999"));
        let neighbour = found(jsontree::describe_node(&s, "pgb_tree_edit", "docs", &cols, &k1, "doc", &[k("items"), i(1), k("i")]).await?);
        assert_eq!(neighbour.preview.as_deref(), Some("2"), "the neighbouring element is untouched");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_edit CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn delete_append_and_insert_a_new_key() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_edit2").await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_edit2", "docs").await?;
        let k1 = key(1);
        let xmin = || jsontree::read_xmin(&s, "pgb_tree_edit2", "docs", &cols, &k1);

        // Delete an existing key.
        let x = xmin().await?.unwrap();
        jsontree::delete_at_path(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("b")], &x).await.map_err(UserFacingError::from)?;
        let root = found(jsontree::describe_node(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[]).await?);
        assert_eq!(root.count, Some(2));
        assert_eq!(jsontree::describe_node(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("b")]).await?, NodeLookup::Missing);

        // Append to the array by writing at its current length.
        let x = xmin().await?.unwrap();
        jsontree::set_at_path(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("c"), i(3)], "40", &x).await.map_err(UserFacingError::from)?;
        let c = found(jsontree::describe_node(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("c")]).await?);
        assert_eq!(c.count, Some(4));
        let (items, _) = jsontree::list_children(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("c")], NodeKind::Array, 3, 10).await?;
        assert_eq!(items[0].info.preview.as_deref(), Some("40"));

        // Insert a brand-new object key with an object value.
        let x = xmin().await?.unwrap();
        jsontree::set_at_path(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("fresh")], "{\"nested\": true}", &x)
            .await
            .map_err(UserFacingError::from)?;
        let fresh = found(jsontree::describe_node(&s, "pgb_tree_edit2", "docs", &cols, &k1, "doc", &[k("fresh")]).await?);
        assert_eq!((fresh.kind, fresh.count), (NodeKind::Object, Some(1)));
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_edit2 CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn whole_value_replace_works_on_any_large_capable_type_including_plain_text() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_whole").await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_whole", "docs").await?;
        let k1 = key(1);

        // Whole-column replace on jsonb (empty path).
        let x = jsontree::read_xmin(&s, "pgb_tree_whole", "docs", &cols, &k1).await?.unwrap();
        jsontree::set_at_path(&s, "pgb_tree_whole", "docs", &cols, &k1, "doc", &[], "{\"replaced\": true}", &x).await.map_err(UserFacingError::from)?;
        let root = found(jsontree::describe_node(&s, "pgb_tree_whole", "docs", &cols, &k1, "doc", &[]).await?);
        assert_eq!(root.count, Some(1));

        // Whole-column replace on a plain `text` column: not jsonb, no JSON validation, any text ok.
        let x = jsontree::read_xmin(&s, "pgb_tree_whole", "docs", &cols, &k1).await?.unwrap();
        jsontree::set_at_path(&s, "pgb_tree_whole", "docs", &cols, &k1, "note", &[], "not json at all { } [", &x)
            .await
            .map_err(UserFacingError::from)?;
        let note: String = s.client().query_one("SELECT note FROM pgb_tree_whole.docs WHERE id = 1", &[]).await?.get(0);
        assert_eq!(note, "not json at all { } [");

        // A non-empty path is refused on a non-jsonb column, before touching the network effects.
        let err = jsontree::set_at_path(&s, "pgb_tree_whole", "docs", &cols, &k1, "note", &[k("x")], "1", "0").await.unwrap_err();
        assert!(matches!(err, SetError::Invalid(_)));
        let err = jsontree::delete_at_path(&s, "pgb_tree_whole", "docs", &cols, &k1, "note", &[k("x")], "0").await.unwrap_err();
        assert!(matches!(err, SetError::Invalid(_)));
        // An empty path is refused for delete.
        let err = jsontree::delete_at_path(&s, "pgb_tree_whole", "docs", &cols, &k1, "doc", &[], "0").await.unwrap_err();
        assert!(matches!(err, SetError::Invalid(_)));
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_whole CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn invalid_json_is_rejected_before_any_write_and_bad_xmin_is_a_conflict() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_bad").await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_bad", "docs").await?;
        let k1 = key(1);
        let x = jsontree::read_xmin(&s, "pgb_tree_bad", "docs", &cols, &k1).await?.unwrap();

        let err = jsontree::set_at_path(&s, "pgb_tree_bad", "docs", &cols, &k1, "doc", &[k("a")], "{not valid", &x).await.unwrap_err();
        assert!(matches!(err, SetError::Invalid(_)), "{err:?}");
        let root = found(jsontree::describe_node(&s, "pgb_tree_bad", "docs", &cols, &k1, "doc", &[]).await?);
        assert_eq!(root.count, Some(3), "nothing changed");

        // Someone else changes the row; our xmin is now stale.
        s.client().batch_execute("UPDATE pgb_tree_bad.docs SET doc = doc || '{\"z\": 1}' WHERE id = 1").await?;
        let err = jsontree::set_at_path(&s, "pgb_tree_bad", "docs", &cols, &k1, "doc", &[k("a")], "2", &x).await.unwrap_err();
        assert_eq!(err, SetError::Conflict);
        let a = found(jsontree::describe_node(&s, "pgb_tree_bad", "docs", &cols, &k1, "doc", &[k("a")]).await?);
        assert_eq!(a.preview.as_deref(), Some("1"), "the stale write did not go through");

        // Deleting with a stale xmin is also a conflict, not silently a no-op success.
        let err = jsontree::delete_at_path(&s, "pgb_tree_bad", "docs", &cols, &k1, "doc", &[k("a")], &x).await.unwrap_err();
        assert_eq!(err, SetError::Conflict);
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_bad CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn read_node_text_caps_and_row_states_are_reported() {
    let s = Session::connect(&admin()).await.unwrap();
    make_tree_table(&s, "pgb_tree_read").await;
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_read", "docs").await?;
        let big = jsontree::read_node_text(&s, "pgb_tree_read", "docs", &cols, &key(1), "doc", &[k("b")], 5).await?;
        assert!(matches!(big, CellFetch::TooLarge { .. }), "{big:?}");
        let small = jsontree::read_node_text(&s, "pgb_tree_read", "docs", &cols, &key(1), "doc", &[k("a")], 100).await?;
        assert_eq!(small, CellFetch::Full("1".into()));
        assert_eq!(jsontree::read_node_text(&s, "pgb_tree_read", "docs", &cols, &key(1), "doc", &[k("nope")], 100).await?, CellFetch::Null);
        assert_eq!(jsontree::read_node_text(&s, "pgb_tree_read", "docs", &cols, &key(999), "doc", &[], 100).await?, CellFetch::RowMissing);
        assert_eq!(jsontree::read_xmin(&s, "pgb_tree_read", "docs", &cols, &key(999)).await?, None);
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_read CASCADE").await.unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs a seeded server: set PGB_TEST_URL and run with --ignored"]
async fn a_readonly_role_cannot_write_a_path_and_the_denial_is_classified() {
    let base = admin();
    let s = Session::connect(&base).await.unwrap();
    make_tree_table(&s, "pgb_tree_ro").await;
    s.client().batch_execute("GRANT USAGE ON SCHEMA pgb_tree_ro TO pgb_readonly; GRANT SELECT ON pgb_tree_ro.docs TO pgb_readonly;").await.unwrap();
    let result = async {
        let cols = data::table_columns(&s, "pgb_tree_ro", "docs").await?;
        let k1 = key(1);
        let ro = Session::connect(&as_user(&base, "pgb_readonly", "readonly")).await.unwrap();
        // Reading is fine.
        let root = found(jsontree::describe_node(&ro, "pgb_tree_ro", "docs", &cols, &k1, "doc", &[]).await?);
        assert_eq!(root.count, Some(3));
        let x = jsontree::read_xmin(&ro, "pgb_tree_ro", "docs", &cols, &k1).await?.unwrap();
        // Writing is refused by the server and classified as a permission error, not a conflict.
        let err = jsontree::set_at_path(&ro, "pgb_tree_ro", "docs", &cols, &k1, "doc", &[k("a")], "2", &x).await.unwrap_err();
        let SetError::Failed(e) = err else { panic!("expected Failed, got {err:?}") };
        assert_eq!(e.kind, ErrorKind::Permission, "{e:?}");
        Ok::<_, UserFacingError>(())
    }
    .await;
    s.client().batch_execute("DROP SCHEMA pgb_tree_ro CASCADE").await.unwrap();
    result.unwrap();
}
