//! v7.37.17 (17.6 siblings) — MySQL get_format + convert_tz close
//! the MySQL date-function surface.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn get_format_doc_vectors() {
    let mut e = Engine::new();
    // MySQL doc vectors.
    assert_eq!(
        text(&first(&mut e, "SELECT get_format(DATE, 'USA')")),
        "%m.%d.%Y"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT get_format(DATE, 'EUR')")),
        "%d.%m.%Y"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT get_format(DATETIME, 'ISO')")),
        "%Y-%m-%d %H.%i.%s"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT get_format(TIME, 'USA')")),
        "%h:%i:%s %p"
    );
    // MySQL doc example: DATE_FORMAT + GET_FORMAT compose.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format('2003-10-03', get_format(DATE, 'EUR'))"
        )),
        "03.10.2003"
    );
    // Unknown region → NULL.
    assert!(matches!(
        first(&mut e, "SELECT get_format(DATE, 'XYZ')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn convert_tz_offset_forms() {
    let mut e = Engine::new();
    // MySQL doc vector: CONVERT_TZ('2004-01-01 12:00:00', '+00:00',
    // '+10:00') → '2004-01-01 22:00:00'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(convert_tz('2004-01-01 12:00:00', '+00:00', '+10:00'), \
             '%Y-%m-%d %H:%i:%s')"
        )),
        "2004-01-01 22:00:00"
    );
    // Negative offsets across a date boundary.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(convert_tz('2004-01-01 02:00:00', '+03:00', '-05:00'), \
             '%Y-%m-%d %H:%i:%s')"
        )),
        "2003-12-31 18:00:00"
    );
    // Named zones without tzdata → NULL (MySQL with unloaded
    // time-zone tables).
    assert!(matches!(
        first(
            &mut e,
            "SELECT convert_tz('2004-01-01 12:00:00', 'GMT', 'MET')"
        ),
        spg_storage::Value::Null
    ));
}
