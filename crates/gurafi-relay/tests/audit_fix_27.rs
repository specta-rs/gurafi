//! Regression test for connection total counts outside the GraphQL Int range.

use gurafi::{IntoValue, gql};
use gurafi_relay::{Connection, ConnectionArgs};

#[derive(Clone, Copy)]
struct ZeroSized;

impl IntoValue for ZeroSized {
    fn into_value(self) -> Result<gql::Value, gql::GraphQLError> {
        Ok(gql::Value::Int(0))
    }
}

fn empty_page_args() -> ConnectionArgs {
    ConnectionArgs {
        first: Some(0),
        ..ConnectionArgs::default()
    }
}

fn assert_zero_page(connection: Connection<ZeroSized>, expected_total_count: Option<i32>) {
    assert!(connection.edges.is_empty());
    assert!(connection.page_info.has_next_page);
    assert!(!connection.page_info.has_previous_page);
    assert!(connection.page_info.start_cursor.is_none());
    assert!(connection.page_info.end_cursor.is_none());
    assert_eq!(connection.total_count, expected_total_count);

    let serialized = connection.into_value().expect("serialize connection");
    let object = serialized.as_object().expect("connection object");
    match expected_total_count {
        Some(expected) => assert_eq!(
            object.get("totalCount").and_then(gql::Value::as_i32),
            Some(expected)
        ),
        None => assert!(
            object.get("totalCount").is_none(),
            "an unrepresentable totalCount must be omitted, not wrapped or null"
        ),
    }
}

#[test]
fn total_count_at_i32_max_is_preserved() {
    // Zero-sized elements make the boundary fixture constant-space: no multi-gigabyte
    // allocation or item traversal is needed to exercise the usize-to-i32 conversion.
    let items = vec![ZeroSized; i32::MAX as usize];
    let args = empty_page_args();

    let by_index = Connection::from_slice_by_index(&items, &args).expect("valid index page");
    let by_slice =
        Connection::from_slice(&items, &args, |_: &ZeroSized| "unused").expect("valid slice page");

    assert_zero_page(by_index, Some(i32::MAX));
    assert_zero_page(by_slice, Some(i32::MAX));
}

#[test]
fn unrepresentable_total_count_is_omitted_instead_of_wrapped() {
    let Some(len) = (i32::MAX as usize).checked_add(1) else {
        // A 32-bit usize cannot represent the unrepresentable-count boundary.
        return;
    };
    let items = vec![ZeroSized; len];
    let args = empty_page_args();

    let by_index = Connection::from_slice_by_index(&items, &args).expect("valid index page");
    let by_slice =
        Connection::from_slice(&items, &args, |_: &ZeroSized| "unused").expect("valid slice page");

    assert_zero_page(by_index, None);
    assert_zero_page(by_slice, None);
}
