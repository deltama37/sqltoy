//! Rule-based plans, `EXPLAIN`, and planner on/off equivalence.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{Database, QueryResult, Value};

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqltoy-plan-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(sqltoy::wal_path(&path));
    path
}

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(sqltoy::wal_path(&self.0));
    }
}

fn open(label: &str) -> (Temp, Database) {
    let path = temp_path(label);
    let db = Database::open(&path).unwrap();
    (Temp(path), db)
}

fn exec(db: &mut Database, sql: &str) {
    db.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

fn plan_lines(db: &mut Database, sql: &str) -> Vec<String> {
    let results = db.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    let QueryResult::Rows { columns, rows } = &results[0] else {
        panic!("{sql}: {results:?}");
    };
    assert_eq!(columns, &["QUERY PLAN".to_string()], "{sql}");
    rows.iter()
        .map(|row| match &row[0] {
            Value::Text(text) => text.clone(),
            other => panic!("{sql}: {other:?}"),
        })
        .collect()
}

fn expect_plan(db: &mut Database, sql: &str, expected: &[&str]) {
    assert_eq!(plan_lines(db, sql), expected, "{sql}");
}

fn setup(db: &mut Database) {
    exec(
        db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, last_order INTEGER); \
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, item TEXT); \
         CREATE TABLE extra (id INTEGER PRIMARY KEY, n INTEGER); \
         CREATE TABLE plain (id INTEGER, user_id INTEGER)",
    );
}

#[test]
fn explain_text_matches_each_rule() {
    let (_temp, mut db) = open("text");
    setup(&mut db);

    expect_plan(
        &mut db,
        "EXPLAIN SELECT name FROM users WHERE id = 1",
        &[
            "Project name",
            "  Filter (id = 1)",
            "    IndexLookup users (id = 1)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT name FROM users WHERE 1 = id",
        &[
            "Project name",
            "  Filter (1 = id)",
            "    IndexLookup users (id = 1)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id = 1 + 2",
        &[
            "Project id",
            "  Filter (id = (1 + 2))",
            "    IndexLookup users (id = 3)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id >= 3 AND id < 9",
        &[
            "Project id",
            "  Filter ((id >= 3) AND (id < 9))",
            "    IndexScan users (id >= 3 AND id < 9)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE 5 < id AND id <= 10",
        &[
            "Project id",
            "  Filter ((5 < id) AND (id <= 10))",
            "    IndexScan users (id > 5 AND id <= 10)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id >= 3",
        &[
            "Project id",
            "  Filter (id >= 3)",
            "    IndexScan users (id >= 3)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id > 10 AND id < 5",
        &[
            "Project id",
            "  Filter ((id > 10) AND (id < 5))",
            "    IndexScan users (id > 10 AND id < 5)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users ORDER BY id LIMIT 5",
        &["Limit 5", "  Project id", "    IndexScan users"],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id AS k FROM users ORDER BY k",
        &["Project id", "  IndexScan users"],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT name, id FROM users ORDER BY 2",
        &["Project name, id", "  IndexScan users"],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users u ORDER BY u.id",
        &["Project id", "  IndexScan users AS u"],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users ORDER BY id DESC",
        &["Sort id DESC", "  Project id", "    SeqScan users"],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id >= 3 ORDER BY id DESC",
        &[
            "Sort id DESC",
            "  Project id",
            "    Filter (id >= 3)",
            "      IndexScan users (id >= 3)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id = 1 OR id = 2",
        &[
            "Project id",
            "  Filter ((id = 1) OR (id = 2))",
            "    SeqScan users",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name, o.item \
         FROM users u INNER JOIN orders o ON u.id = o.user_id \
         WHERE u.id >= 3 AND o.id = 10 AND u.name = o.item",
        &[
            "Project u.name, o.item",
            "  Filter (u.name = o.item)",
            "    NestedLoopJoin inner ON (u.id = o.user_id)",
            "      Filter (u.id >= 3)",
            "        IndexScan users AS u (id >= 3)",
            "      Filter (o.id = 10)",
            "        IndexLookup orders AS o (id = 10)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name, o.item \
         FROM users u LEFT JOIN orders o ON u.id = o.user_id \
         WHERE o.id = 5",
        &[
            "Project u.name, o.item",
            "  Filter (o.id = 5)",
            "    NestedLoopJoin left ON (u.id = o.user_id)",
            "      SeqScan users AS u",
            "      SeqScan orders AS o",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.id \
         FROM users u LEFT JOIN orders o ON u.id = o.user_id \
         WHERE u.name = o.item",
        &[
            "Project u.id",
            "  Filter (u.name = o.item)",
            "    NestedLoopJoin left ON (u.id = o.user_id)",
            "      SeqScan users AS u",
            "      SeqScan orders AS o",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name, o.item \
         FROM users u INNER JOIN orders o ON o.id = u.last_order \
         WHERE u.id >= 3 LIMIT 10",
        &[
            "Limit 10",
            "  Project u.name, o.item",
            "    IndexNestedLoopJoin inner orders AS o (o.id = u.last_order)",
            "      Filter (u.id >= 3)",
            "        IndexScan users AS u (id >= 3)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON o.id = u.last_order",
        &[
            "Project u.name",
            "  IndexNestedLoopJoin left orders AS o (o.id = u.last_order)",
            "    SeqScan users AS u",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name FROM users u INNER JOIN orders o ON o.id = o.user_id",
        &[
            "Project u.name",
            "  NestedLoopJoin inner ON (o.id = o.user_id)",
            "    SeqScan users AS u",
            "    SeqScan orders AS o",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name FROM users u INNER JOIN orders o ON o.id = u.id + o.user_id",
        &[
            "Project u.name",
            "  NestedLoopJoin inner ON (o.id = (u.id + o.user_id))",
            "    SeqScan users AS u",
            "    SeqScan orders AS o",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.id FROM users u INNER JOIN plain p ON p.id = u.id",
        &[
            "Project u.id",
            "  NestedLoopJoin inner ON (p.id = u.id)",
            "    SeqScan users AS u",
            "    SeqScan plain AS p",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.id \
         FROM users u \
         INNER JOIN orders o ON u.id = o.user_id \
         INNER JOIN extra e ON o.id = e.n \
         WHERE u.id = 1 AND e.n = 2 AND o.user_id = e.n",
        &[
            "Project u.id",
            "  Filter (o.user_id = e.n)",
            "    NestedLoopJoin inner ON (o.id = e.n)",
            "      NestedLoopJoin inner ON (u.id = o.user_id)",
            "        Filter (u.id = 1)",
            "          IndexLookup users AS u (id = 1)",
            "        SeqScan orders AS o",
            "      Filter (e.n = 2)",
            "        SeqScan extra AS e",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN UPDATE users SET name = 'a' WHERE id = 1 AND name = 'b'",
        &[
            "Filter ((id = 1) AND (name = 'b'))",
            "  IndexLookup users (id = 1)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN DELETE FROM users WHERE id >= 3 AND id < 10",
        &[
            "Filter ((id >= 3) AND (id < 10))",
            "  IndexScan users (id >= 3 AND id < 10)",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN DELETE FROM orders WHERE id = 4 OR item IS NULL",
        &["Filter ((id = 4) OR (item IS NULL))", "  SeqScan orders"],
    );

    db.set_planner_enabled(false);
    expect_plan(
        &mut db,
        "EXPLAIN SELECT id FROM users WHERE id = 1 ORDER BY id",
        &[
            "Sort id",
            "  Project id",
            "    Filter (id = 1)",
            "      SeqScan users",
        ],
    );
    expect_plan(
        &mut db,
        "EXPLAIN SELECT u.name FROM users u INNER JOIN orders o ON o.id = u.last_order",
        &[
            "Project u.name",
            "  NestedLoopJoin inner ON (o.id = u.last_order)",
            "    SeqScan users AS u",
            "    SeqScan orders AS o",
        ],
    );
}

#[test]
fn explain_does_not_execute_and_analyze_is_select_only() {
    let (_temp, mut db) = open("noexec");
    setup(&mut db);
    exec(
        &mut db,
        "INSERT INTO users VALUES (1, 'Ann', 20, 10), (2, 'Bea', NULL, NULL)",
    );
    exec(
        &mut db,
        "EXPLAIN UPDATE users SET name = 'nope' WHERE id = 1",
    );
    exec(&mut db, "EXPLAIN SELECT name FROM users WHERE id = 1 / 0");
    let QueryResult::Rows { rows, .. } =
        &db.execute("SELECT name FROM users WHERE id = 1").unwrap()[0]
    else {
        panic!("select");
    };
    assert_eq!(rows, &vec![vec![Value::Text("Ann".to_string())]]);

    let err = db
        .execute("EXPLAIN ANALYZE UPDATE users SET name = 'nope' WHERE id = 1")
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "EXPLAIN ANALYZE supports only SELECT");
    let err = db
        .execute("EXPLAIN ANALYZE DELETE FROM users WHERE id = 1")
        .unwrap_err();
    assert_eq!(err.to_string(), "EXPLAIN ANALYZE supports only SELECT");
    let QueryResult::Rows { rows, .. } =
        &db.execute("SELECT name FROM users WHERE id = 1").unwrap()[0]
    else {
        panic!("select");
    };
    assert_eq!(rows, &vec![vec![Value::Text("Ann".to_string())]]);

    exec(&mut db, "BEGIN");
    exec(&mut db, "EXPLAIN SELECT id FROM users WHERE id = 1");
    exec(&mut db, "INSERT INTO users VALUES (3, 'Cam', 1, NULL)");
    let lines = plan_lines(
        &mut db,
        "EXPLAIN ANALYZE SELECT name FROM users WHERE id = 3",
    );
    assert!(
        lines.iter().any(|line| line.contains("(rows=1)")),
        "{lines:?}"
    );
    exec(&mut db, "ROLLBACK");
    let QueryResult::Rows { rows, .. } =
        &db.execute("SELECT id FROM users WHERE id = 3").unwrap()[0]
    else {
        panic!("select");
    };
    assert!(rows.is_empty());

    exec(&mut db, "BEGIN");
    exec(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM users ORDER BY id LIMIT 1",
    );
    exec(&mut db, "COMMIT");
}

#[test]
fn explain_analyze_counts_rows_and_reads_fewer_pages() {
    let (_temp, mut db) = open("analyze");
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
    );
    exec(
        &mut db,
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')",
    );
    let lines = plan_lines(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM t ORDER BY id LIMIT 2",
    );
    assert_eq!(
        &lines[..lines.len() - 1],
        &[
            "Limit 2 (rows=2)",
            "  Project id (rows=2)",
            "    IndexScan t (rows=2)",
        ]
    );
    assert!(
        lines.last().unwrap().starts_with("Pages read: "),
        "{lines:?}"
    );

    let lines = plan_lines(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        &lines[..lines.len() - 1],
        &[
            "Limit 2 OFFSET 1 (rows=2)",
            "  Project id (rows=3)",
            "    IndexScan t (rows=3)",
        ]
    );

    let lines = plan_lines(&mut db, "EXPLAIN ANALYZE SELECT id FROM t WHERE id >= 3");
    assert_eq!(
        &lines[..lines.len() - 1],
        &[
            "Project id (rows=3)",
            "  Filter (id >= 3) (rows=3)",
            "    IndexScan t (id >= 3) (rows=3)",
        ]
    );

    let lines = plan_lines(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM t WHERE id > 10 AND id < 5",
    );
    assert_eq!(
        &lines[..lines.len() - 1],
        &[
            "Project id (rows=0)",
            "  Filter ((id > 10) AND (id < 5)) (rows=0)",
            "    IndexScan t (id > 10 AND id < 5) (rows=0)",
        ]
    );
    assert_eq!(lines.last().unwrap(), "Pages read: 0");

    db.set_planner_enabled(false);
    let lines = plan_lines(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM t ORDER BY id LIMIT 2",
    );
    assert_eq!(
        &lines[..lines.len() - 1],
        &[
            "Limit 2 (rows=2)",
            "  Sort id (rows=2)",
            "    Project id (rows=5)",
            "      SeqScan t (rows=5)",
        ]
    );
    db.set_planner_enabled(true);

    exec(
        &mut db,
        "CREATE TABLE wide (id INTEGER PRIMARY KEY, note TEXT)",
    );
    let note = "n".repeat(2500);
    for id in 1..=250 {
        exec(
            &mut db,
            &format!("INSERT INTO wide VALUES ({id}, '{note}')"),
        );
    }
    let limited = pages_read_of(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM wide ORDER BY id LIMIT 5",
    );
    let ranged = pages_read_of(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM wide WHERE id >= 100 AND id < 120",
    );
    db.set_planner_enabled(false);
    let limited_off = pages_read_of(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM wide ORDER BY id LIMIT 5",
    );
    let ranged_off = pages_read_of(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM wide WHERE id >= 100 AND id < 120",
    );
    assert!(
        limited * 8 < limited_off,
        "ORDER BY id LIMIT 5 read {limited} pages, planner off read {limited_off}"
    );
    assert!(
        ranged * 4 < ranged_off,
        "range read {ranged} pages, planner off read {ranged_off}"
    );
    assert!(limited < 40, "limit read {limited}");
    assert!(ranged < 80, "range read {ranged}");
}

fn pages_read_of(db: &mut Database, sql: &str) -> u64 {
    let lines = plan_lines(db, sql);
    let last = lines.last().unwrap();
    last.trim_start_matches("Pages read: ")
        .parse()
        .unwrap_or_else(|_| panic!("{last}"))
}

#[test]
fn planner_matches_with_the_planner_disabled() {
    let (_on_temp, mut on) = open("diff-on");
    let (_off_temp, mut off) = open("diff-off");
    let setup_sql = sample_data();
    exec(&mut on, &setup_sql);
    exec(&mut off, &setup_sql);
    off.set_planner_enabled(false);

    let mut rng = Rng(0xF606_0015);
    for index in 0..360 {
        let (sql, ordered) = if index < 240 {
            single_select(&mut rng)
        } else {
            join_select(&mut rng)
        };
        let left = on.execute(&sql);
        let right = off.execute(&sql);
        match (left, right) {
            (Ok(left), Ok(right)) => {
                let QueryResult::Rows { rows: left, .. } = &left[0] else {
                    panic!("{sql}: {left:?}");
                };
                let QueryResult::Rows { rows: right, .. } = &right[0] else {
                    panic!("{sql}: {right:?}");
                };
                if ordered {
                    assert_eq!(left, right, "{sql}");
                } else {
                    assert_eq!(canonical(left), canonical(right), "{sql}");
                }
            }
            (Err(left), Err(right)) => {
                assert_eq!(left.kind(), right.kind(), "{sql}");
                assert_eq!(left.to_string(), right.to_string(), "{sql}");
            }
            (left, right) => panic!("{sql}\n{left:?}\n{right:?}"),
        }
    }

    let (_left_temp, mut left) = open("mut-on");
    let (_right_temp, mut right) = open("mut-off");
    exec(&mut left, &setup_sql);
    exec(&mut right, &setup_sql);
    right.set_planner_enabled(false);
    for _ in 0..40 {
        let sql = mutate(&mut rng);
        let left_result = left.execute(&sql);
        let right_result = right.execute(&sql);
        match (left_result, right_result) {
            (Ok(left_result), Ok(right_result)) => {
                assert_eq!(left_result, right_result, "{sql}");
            }
            (Err(left_err), Err(right_err)) => {
                assert_eq!(left_err.to_string(), right_err.to_string(), "{sql}");
            }
            (left_result, right_result) => panic!("{sql}\n{left_result:?}\n{right_result:?}"),
        }
        assert_eq!(
            dump(&mut left, "SELECT id, name, n, flag FROM u ORDER BY id"),
            dump(&mut right, "SELECT id, name, n, flag FROM u ORDER BY id"),
            "{sql}"
        );
        assert_eq!(
            dump(&mut left, "SELECT id, u_id, sku, qty FROM o ORDER BY id"),
            dump(&mut right, "SELECT id, u_id, sku, qty FROM o ORDER BY id"),
            "{sql}"
        );
    }
}

fn sample_data() -> String {
    let mut sql = String::from(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT, n INTEGER, flag INTEGER); \
         CREATE TABLE o (id INTEGER PRIMARY KEY, u_id INTEGER, sku TEXT, qty INTEGER); ",
    );
    let names = ["ann", "bea", "cam", "dee"];
    for id in 1..=30 {
        let name = if id % 7 == 0 {
            "NULL".to_string()
        } else if id % 11 == 0 {
            "''".to_string()
        } else {
            format!("'{}'", names[id % names.len()])
        };
        let n = if id % 5 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", id % 13)
        };
        let flag = id % 2;
        sql.push_str(&format!(
            "INSERT INTO u VALUES ({id}, {name}, {n}, {flag}); "
        ));
    }
    let skus = ["pen", "cup", "mug"];
    for id in 1..=60 {
        let u_id = match id % 6 {
            0 => "NULL".to_string(),
            1 => "100".to_string(),
            _ => format!("{}", (id % 30) + 1),
        };
        let sku = if id % 8 == 0 {
            "NULL".to_string()
        } else {
            format!("'{}'", skus[id % skus.len()])
        };
        let qty = if id % 4 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", id % 9)
        };
        sql.push_str(&format!(
            "INSERT INTO o VALUES ({id}, {u_id}, {sku}, {qty}); "
        ));
    }
    sql
}

fn single_select(rng: &mut Rng) -> (String, bool) {
    let pred = |rng: &mut Rng| match rng.gen(10) {
        0 => format!("id = {}", rng.gen(40)),
        1 => format!("id >= {}", rng.gen(32)),
        2 => format!("id < {}", rng.gen(32)),
        3 => format!("{} < id", rng.gen(20)),
        4 => format!("id > {} AND id <= {}", rng.gen(15), 10 + rng.gen(25)),
        5 => format!("n = {}", rng.gen(14)),
        6 => "n IS NULL".to_string(),
        7 => "n IS NOT NULL".to_string(),
        8 => "name = 'ann'".to_string(),
        _ => format!("flag = {}", rng.gen(2)),
    };
    let filter = if rng.gen(6) == 0 {
        String::new()
    } else if rng.gen(3) == 0 {
        format!("WHERE {} OR {}", pred(rng), pred(rng))
    } else {
        format!("WHERE {} AND {}", pred(rng), pred(rng))
    };
    let (order, ordered) = match rng.gen(6) {
        0 => (String::new(), false),
        1 => ("ORDER BY id".to_string(), true),
        2 => ("ORDER BY id DESC".to_string(), true),
        3 => ("ORDER BY id ASC".to_string(), true),
        4 => ("ORDER BY name, id".to_string(), true),
        _ => ("ORDER BY n DESC, id".to_string(), true),
    };
    let limit = if ordered && rng.gen(2) == 0 {
        format!("LIMIT {} OFFSET {}", rng.gen(12), rng.gen(4))
    } else {
        String::new()
    };
    (
        format!("SELECT id, name, n, flag FROM u {filter} {order} {limit}"),
        ordered,
    )
}

fn join_select(rng: &mut Rng) -> (String, bool) {
    let kind = if rng.gen(2) == 0 { "INNER" } else { "LEFT" };
    let on = match rng.gen(5) {
        0 => "o.id = u.id",
        1 => "u.id = o.u_id",
        2 => "o.u_id = u.id",
        3 => "o.id = u.n",
        _ => "u.n = o.qty",
    };
    let pred = |rng: &mut Rng| match rng.gen(8) {
        0 => format!("u.id >= {}", rng.gen(30)),
        1 => format!("o.id < {}", rng.gen(40)),
        2 => "o.sku IS NULL".to_string(),
        3 => "o.id IS NULL".to_string(),
        4 => "u.n IS NULL".to_string(),
        5 => "u.n = o.qty".to_string(),
        6 => format!("u.flag = {}", rng.gen(2)),
        _ => format!("o.qty > {}", rng.gen(6)),
    };
    let filter = if rng.gen(4) == 0 {
        String::new()
    } else if kind == "LEFT" && rng.gen(3) == 0 {
        "WHERE o.id IS NULL".to_string()
    } else if rng.gen(3) == 0 {
        format!("WHERE {} OR {}", pred(rng), pred(rng))
    } else {
        format!("WHERE {} AND {}", pred(rng), pred(rng))
    };
    let (order, ordered) = if rng.gen(2) == 0 {
        ("ORDER BY u.id, o.id".to_string(), true)
    } else {
        (String::new(), false)
    };
    let limit = if ordered && rng.gen(2) == 0 {
        format!("LIMIT {} OFFSET {}", rng.gen(10), rng.gen(3))
    } else {
        String::new()
    };
    (
        format!(
            "SELECT u.id, u.name, o.id, o.sku, o.qty FROM u {kind} JOIN o ON {on} {filter} {order} {limit}"
        ),
        ordered,
    )
}

fn mutate(rng: &mut Rng) -> String {
    match rng.gen(6) {
        0 => format!(
            "UPDATE u SET n = {} WHERE id >= {} AND id < {}",
            rng.gen(5),
            rng.gen(20),
            10 + rng.gen(25)
        ),
        1 => "UPDATE u SET name = 'z' WHERE n IS NULL".to_string(),
        2 => format!(
            "UPDATE u SET flag = {} WHERE id = {}",
            rng.gen(2),
            1 + rng.gen(30)
        ),
        3 => format!(
            "UPDATE o SET qty = 1 WHERE id >= {} AND id < {}",
            rng.gen(30),
            20 + rng.gen(40)
        ),
        4 => format!(
            "DELETE FROM o WHERE id = {} OR sku IS NULL",
            1 + rng.gen(60)
        ),
        _ => format!(
            "DELETE FROM u WHERE flag = {} AND id < {}",
            rng.gen(2),
            5 + rng.gen(20)
        ),
    }
}

fn dump(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = db.execute(sql).unwrap();
    match &results[0] {
        QueryResult::Rows { rows, .. } => rows.clone(),
        other => panic!("{other:?}"),
    }
}

fn canonical(rows: &[Vec<Value>]) -> Vec<String> {
    let mut keys: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
    keys.sort();
    keys
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn gen(&mut self, n: u64) -> usize {
        (self.next() % n) as usize
    }
}

#[test]
fn index_scan_and_inl_respect_visibility() {
    let (_temp, mut db) = open("vis");
    let main = db.default_session();
    exec(
        &mut db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, k INTEGER); \
         CREATE TABLE orders (id INTEGER PRIMARY KEY, item TEXT); \
         INSERT INTO users VALUES (1, 'Ann', 10), (2, 'Bea', 11); \
         INSERT INTO orders VALUES (10, 'pen'), (11, 'cup')",
    );
    let reader = db.create_session();
    let writer = db.create_session();
    db.execute_in(reader, "BEGIN").unwrap();
    db.execute_in(writer, "BEGIN").unwrap();
    db.execute_in(writer, "UPDATE users SET name = 'Ann2' WHERE id = 1")
        .unwrap();
    db.execute_in(writer, "COMMIT").unwrap();
    let old = rows(
        &mut db,
        reader,
        "SELECT name FROM users WHERE id >= 1 AND id < 2",
    );
    assert_eq!(old, vec![vec![text("Ann")]]);
    let ordered = rows(&mut db, reader, "SELECT name FROM users ORDER BY id");
    assert_eq!(ordered, vec![vec![text("Ann")], vec![text("Bea")]]);
    let joined = rows(
        &mut db,
        reader,
        "SELECT o.item FROM users u INNER JOIN orders o ON o.id = u.k WHERE u.id = 1",
    );
    assert_eq!(joined, vec![vec![text("pen")]]);

    db.execute_in(writer, "BEGIN").unwrap();
    db.execute_in(writer, "INSERT INTO orders VALUES (12, 'secret')")
        .unwrap();
    db.execute_in(writer, "INSERT INTO users VALUES (3, 'Cam', 12)")
        .unwrap();
    assert!(rows(
        &mut db,
        reader,
        "SELECT item FROM orders WHERE id >= 12 AND id < 20"
    )
    .is_empty());
    assert!(rows(
        &mut db,
        reader,
        "SELECT name FROM users WHERE id >= 3 AND id < 4"
    )
    .is_empty());
    let own = rows(
        &mut db,
        writer,
        "SELECT item FROM orders WHERE id >= 12 AND id < 20",
    );
    assert_eq!(own, vec![vec![text("secret")]]);
    let inl = rows(
        &mut db,
        reader,
        "SELECT o.item FROM users u LEFT JOIN orders o ON o.id = u.k WHERE u.id = 3",
    );
    assert!(inl.is_empty());
    let own_inl = rows(
        &mut db,
        writer,
        "SELECT o.item FROM users u INNER JOIN orders o ON o.id = u.k WHERE u.id = 3",
    );
    assert_eq!(own_inl, vec![vec![text("secret")]]);
    db.execute_in(reader, "COMMIT").unwrap();
    db.execute_in(writer, "ROLLBACK").unwrap();
    assert!(rows(&mut db, main, "SELECT name FROM users WHERE id = 3").is_empty());
    assert_eq!(
        rows(&mut db, main, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("Ann2")]]
    );
}

fn rows(db: &mut Database, session: sqltoy::SessionId, sql: &str) -> Vec<Vec<Value>> {
    let results = db
        .execute_in(session, sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    match &results[0] {
        QueryResult::Rows { rows, .. } => rows.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}
