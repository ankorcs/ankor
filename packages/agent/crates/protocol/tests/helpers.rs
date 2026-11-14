//! The wire strings both ends derive from a token or a contract enum.

use ankor_protocol::helpers::{from_wire_name, node_authorization, wire_name};
use ankor_protocol::types::CloseReason;

#[test]
fn a_node_presents_its_token_as_a_bearer() {
    assert_eq!(node_authorization("abc123"), "Bearer abc123");
}

#[test]
fn an_enum_value_goes_on_the_wire_by_its_contract_name() {
    assert_eq!(wire_name(CloseReason::NodeConnected), "node_connected");
    assert_eq!(from_wire_name::<CloseReason>(&wire_name(CloseReason::HelloTimeout)), Some(CloseReason::HelloTimeout));
}
