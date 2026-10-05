// Copyright (C) 2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use clarity::types::chainstate::StacksBlockId;
use clarity::types::Address;
use clarity::vm::database::{ClaritySerializable, DataMapMetadata, DataVariableMetadata};
use clarity::vm::types::TypeSignature;
use stacks_common::types::chainstate::StacksAddress;

use super::{test_rpc, TEST_CONTRACT_ID};
use crate::net::api::*;
use crate::net::connection::ConnectionOptions;
use crate::net::http::Error as HttpError;
use crate::net::httpcore::{
    HttpRequestContentsExtensions as _, RPCRequestHandler, StacksHttp, StacksHttpRequest,
};
use crate::net::{ProtocolFamily, TipRequest};

#[test]
fn test_try_parse_request() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::9::contract-size".to_string(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32])),
    );
    assert_eq!(
        request.contents().tip_request(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32]))
    );
    let bytes = request.try_serialize().unwrap();

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();
    let mut handler = getclaritymetadata::RPCGetClarityMetadataRequestHandler::new();
    let mut parsed_request = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap();

    // parsed request consumes headers that would not be in a constructed request
    parsed_request.clear_headers();
    let (preamble, contents) = parsed_request.destruct();

    // consumed path args
    assert_eq!(
        handler.clarity_metadata_key,
        Some("vm-metadata::9::contract-size".to_string())
    );
    assert_eq!(handler.contract_identifier, Some(TEST_CONTRACT_ID.clone()));

    assert_eq!(&preamble, request.preamble());

    handler.restart();
    assert!(handler.clarity_metadata_key.is_none());
}

#[test]
fn test_try_parse_invalid_store_type() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::2::contract-size".to_string(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32])),
    );
    assert_eq!(
        request.contents().tip_request(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32]))
    );
    let bytes = request.try_serialize().unwrap();

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();
    let mut handler = getclaritymetadata::RPCGetClarityMetadataRequestHandler::new();
    let parsed_request_err = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap_err();

    assert_eq!(
        parsed_request_err,
        HttpError::DecodeError("Invalid metadata type".to_string()).into()
    );
    handler.restart();
}

#[rstest::rstest]
#[case("vm-metadata::9::contract-invalid-key")]
#[case("vm-metadata::9::contract-header")]
#[case("vm-metadata::9::contract-header-v2")]
fn test_try_parse_invalid_contract_metadata_var_name(#[case] key: &str) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        key.to_string(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32])),
    );
    assert_eq!(
        request.contents().tip_request(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32]))
    );
    let bytes = request.try_serialize().unwrap();

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();
    let mut handler = getclaritymetadata::RPCGetClarityMetadataRequestHandler::new();
    let parsed_request_err = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap_err();

    assert_eq!(
        parsed_request_err,
        HttpError::DecodeError("Invalid metadata var name".to_string()).into()
    );
    handler.restart();
}

#[test]
fn test_try_parse_request_for_analysis() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "analysis".to_string(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32])),
    );
    assert_eq!(
        request.contents().tip_request(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32]))
    );
    let bytes = request.try_serialize().unwrap();

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();
    let mut handler = getclaritymetadata::RPCGetClarityMetadataRequestHandler::new();
    let mut parsed_request = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap();

    // parsed request consumes headers that would not be in a constructed request
    parsed_request.clear_headers();
    let (preamble, contents) = parsed_request.destruct();

    // consumed path args
    assert_eq!(handler.clarity_metadata_key, Some("analysis".to_string()));
    assert_eq!(handler.contract_identifier, Some(TEST_CONTRACT_ID.clone()));

    assert_eq!(&preamble, request.preamble());

    handler.restart();
    assert!(handler.clarity_metadata_key.is_none());
}

#[test]
fn test_try_make_response() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);

    let mut requests = vec![];

    // query invalid metadata key (wrong store type)
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::2::bar".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing contract size metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::9::contract-size".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing data map metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::5::test-map".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing data var metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::6::bar".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing data var metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::6::bar".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing data var metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::6::bar".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query undeclared var metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::6::non-existing-var".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query existing contract size metadata
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::9::contract-size".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // query invalid metadata key (wrong store type)
    let request = StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::2::bar".to_string(),
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // The executable metadata endpoint retains its historical shape after splitting.
    requests.push(StacksHttpRequest::new_getclaritymetadata(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "vm-metadata::9::contract".to_string(),
        TipRequest::UseLatestAnchoredTip,
    ));
    let mut responses = test_rpc(function_name!(), requests);
    let serialized = responses
        .pop()
        .unwrap()
        .decode_clarity_metadata_response()
        .unwrap();
    let context: serde_json::Value = serde_json::from_str(&serialized.data).unwrap();
    assert!(context["contract_context"]["functions"].is_object());
    assert!(context["contract_context"]["variables"].is_object());
    let actual: std::collections::BTreeSet<_> = context["contract_context"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected = std::collections::BTreeSet::from([
        "contract_identifier",
        "variables",
        "functions",
        "defined_traits",
        "implemented_traits",
        "persisted_names",
        "meta_data_map",
        "meta_data_var",
        "meta_nft",
        "meta_ft",
        "data_size",
        "clarity_version",
    ]);
    assert_eq!(actual, expected);
    // Preserve the legacy field order, which a parsed JSON object cannot test.
    let mut last = 0;
    for field in [
        "contract_identifier",
        "variables",
        "functions",
        "defined_traits",
        "implemented_traits",
        "persisted_names",
        "meta_data_map",
        "meta_data_var",
        "meta_nft",
        "meta_ft",
        "data_size",
        "clarity_version",
    ] {
        let position = serialized.data.find(&format!("\"{field}\":")).unwrap();
        assert!(position > last, "RPC field out of order: {field}");
        last = position;
    }

    // unknwnon data var
    let response = responses.remove(0);
    let (preamble, body) = response.destruct();
    assert_eq!(preamble.status_code, 400);

    // contract size metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    assert_eq!(resp.data, "1432");

    // data map metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    let expected = DataMapMetadata {
        key_type: TypeSignature::UIntType,
        value_type: TypeSignature::UIntType,
    };
    assert_eq!(resp.data, expected.serialize());

    // data var metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    let expected = DataVariableMetadata {
        value_type: TypeSignature::IntType,
    };
    assert_eq!(resp.data, expected.serialize());

    // data var metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    let expected = DataVariableMetadata {
        value_type: TypeSignature::IntType,
    };
    assert_eq!(resp.data, expected.serialize());

    // data var metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    let expected = DataVariableMetadata {
        value_type: TypeSignature::IntType,
    };
    assert_eq!(resp.data, expected.serialize());

    // invalid metadata key
    let response = responses.remove(0);
    let (preamble, body) = response.destruct();
    assert_eq!(preamble.status_code, 404);

    // contract size metadata
    let response = responses.remove(0);
    let resp = response.decode_clarity_metadata_response().unwrap();
    assert_eq!(resp.data, "1432");

    // unknwnon data var
    let response = responses.remove(0);
    let (preamble, body) = response.destruct();
    assert_eq!(preamble.status_code, 400);
}
