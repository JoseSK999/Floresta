# SPDX-License-Identifier: MIT OR Apache-2.0

"""
Tests for JSON-RPC request parsing in florestad.

Validates that the RPC server correctly handles:
- Positional (array) parameters
- Named (object) parameters
- Null / omitted parameters
- Default values for optional parameters
- Proper JSON-RPC error codes per the spec (-32700, -32600, -32601, -32602, -32603)
- HTTP status codes (400, 404, 500, 503)
- Methods that require no params vs methods that require params
- JSON-RPC 1.0 and 2.0 version acceptance
- Content-type handling
"""

from test_framework.constants import (
    JSONRPC_ERRCODE_INVALID_PARAMS,
    JSONRPC_ERRCODE_INVALID_REQUEST,
    JSONRPC_ERRCODE_METHOD_NOT_FOUND,
    JSONRPC_ERRMSG_INVALID_VERSION,
    JSONRPC_ERRMSG_METHOD_NOT_FOUND,
    JSONRPC_ERRMSG_MISSING_PARAMS,
    JSONRPC_ERRMSG_WRONG_PARAM_TYPE,
    OPTIONAL_PARAM_METHODS,
    METHODS_REQUIRING_PARAMS,
    NO_PARAM_METHODS,
)


class TestRpcServerRequestParsing:
    """
    Test JSON-RPC request parsing, parameter extraction (positional and named),
    error codes, and edge cases on the florestad RPC server.
    """

    def test_optionalparammethods_omittedparams_succeeds(self, shared_florestad_node):
        """Verify methods with only optional params succeed when params is omitted."""
        for method in OPTIONAL_PARAM_METHODS:
            shared_florestad_node.rpc.ensure_rpc_call_success(method=method)

    def test_optionalparammethods_nullparams_succeeds(self, shared_florestad_node):
        """Verify methods with only optional params succeed when params is explicitly null."""
        for method in OPTIONAL_PARAM_METHODS:
            shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
                {"jsonrpc": "2.0", "id": "test", "method": method, "params": None}
            )

    def test_noparammethods_omittedparams_succeeds(self, shared_florestad_node):
        """Verify all no-param methods succeed when the params field is omitted."""
        for method in NO_PARAM_METHODS:
            shared_florestad_node.rpc.ensure_rpc_call_success(method=method)

    def test_noparammethods_nullparams_succeeds(self, shared_florestad_node):
        """Verify all no-param methods succeed when params is explicitly null."""
        for method in NO_PARAM_METHODS:
            shared_florestad_node.rpc.ensure_rpc_call_success(
                method=method, params=None
            )

    def test_noparammethods_emptyarray_succeeds(self, shared_florestad_node):
        """Verify all no-param methods succeed when params is an empty array."""
        for method in NO_PARAM_METHODS:
            shared_florestad_node.rpc.ensure_rpc_call_success(method=method, params=[])

    def test_positionalparams_validargs_succeeds(self, shared_florestad_node):
        """Verify methods accept valid positional (array) parameters."""
        resp = shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblockhash", params=[0]
        )
        genesis_hash = resp["body"]["result"]

        shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblockheader", params=[genesis_hash]
        )

        shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblock", params=[genesis_hash, 1]
        )

    def test_namedparams_validargs_succeeds(self, shared_florestad_node):
        """Verify methods accept valid named (object) parameters."""
        resp = shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblockhash", params={"block_height": 0}
        )
        genesis_hash = resp["body"]["result"]

        shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblockheader", params={"block_hash": genesis_hash}
        )

        shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblock", params={"block_hash": genesis_hash, "verbosity": 0}
        )

    def test_optionalparams_omitted_usesdefaults(self, shared_florestad_node):
        """Verify omitted optional parameters fall back to their defaults."""
        genesis_hash = shared_florestad_node.rpc.get_bestblockhash()

        resp_default = shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblock", params=[genesis_hash]
        )
        result = resp_default["body"]["result"]
        # Check that the default verbosity was enabled.
        assert "hash" in result
        assert "tx" in result

        resp_explicit = shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblock", params=[genesis_hash, 1]
        )
        assert resp_default["body"]["result"] == resp_explicit["body"]["result"]

        resp = shared_florestad_node.rpc.ensure_rpc_call_success(
            method="getblock", params={"block_hash": genesis_hash}
        )
        assert resp_default["body"]["result"] == resp_explicit["body"]["result"]
        assert "hash" in resp["body"]["result"]

    def test_unknownmethod_anyparams_returnsmethodnotfound(self, shared_florestad_node):
        """Verify unknown methods return METHOD_NOT_FOUND (-32601)."""
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="nonexistent_method",
            params=[],
            expected_status_code=404,
            expected_rpcerror_code=JSONRPC_ERRCODE_METHOD_NOT_FOUND,
            expected_message=JSONRPC_ERRMSG_METHOD_NOT_FOUND,
        )

    def test_requiredparams_missing_returnsinvalidparams(self, shared_florestad_node):
        """Verify missing required parameters return INVALID_PARAMS (-32602)."""
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="getblockhash",
            params=[],
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
            expected_message=JSONRPC_ERRMSG_MISSING_PARAMS,
        )

        # {} is an empty object, so it should be accepted as an object
        # but raise that is missing the fields
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="getblockhash",
            params={},
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
            expected_message=JSONRPC_ERRMSG_MISSING_PARAMS,
        )

    def test_paramtypes_wrongtype_returnsinvalidparams(self, shared_florestad_node):
        """Verify wrong parameter types return INVALID_PARAMS (-32602)."""

        # getblockhash expects a number, but "not_a_number" is a string - params must be array
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="getblockhash",
            params=["not_a_number"],
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
            expected_message=JSONRPC_ERRMSG_WRONG_PARAM_TYPE,
        )

        # getblock hash expects a string, but 12345 is a number - params must be array
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="getblock",
            params=[12345],
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
            expected_message=JSONRPC_ERRMSG_WRONG_PARAM_TYPE,
        )

        genesis_hash = shared_florestad_node.rpc.get_bestblockhash()
        shared_florestad_node.rpc.ensure_rpc_call_error(
            method="getblock",
            params=[genesis_hash, "invalid_verbosity"],
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
            expected_message=JSONRPC_ERRMSG_WRONG_PARAM_TYPE,
        )

    def test_jsonrpcversion_invalid_returnsrejection(self, shared_florestad_node):
        """Verify invalid jsonrpc versions are rejected and valid ones accepted."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_error(
            {"jsonrpc": "3.0", "id": "test", "method": "getblockcount", "params": []},
            expected_status_code=400,
            expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_REQUEST,
            expected_message=JSONRPC_ERRMSG_INVALID_VERSION,
        )

        for version in ["1.0", "2.0"]:
            shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
                {
                    "jsonrpc": version,
                    "id": "test",
                    "method": "getblockcount",
                    "params": [],
                },
            )

        shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
            {"id": "test", "method": "getblockcount"}
        )

    def test_parammethods_omittedparams_returnserror(self, shared_florestad_node):
        """Verify methods that require params fail when params are omitted."""
        for method in METHODS_REQUIRING_PARAMS:
            shared_florestad_node.rpc.ensure_rpc_call_error(
                method=method,
                expected_status_code=400,
                expected_rpcerror_code=JSONRPC_ERRCODE_INVALID_PARAMS,
                expected_message=JSONRPC_ERRMSG_MISSING_PARAMS,
            )

    def test_responsestructure_success_matchesjsonrpcspec(self, shared_florestad_node):
        """Verify successful responses match the JSON-RPC spec structure."""
        resp = shared_florestad_node.rpc.noraise_raw_request(
            {"jsonrpc": "2.0", "id": "struct_test", "method": "getblockcount"},
        )
        body = resp["body"]
        assert "result" in body
        assert "id" in body
        assert body["id"] == "struct_test"
        assert body.get("result") is not None

    def test_responsestructure_error_matchesjsonrpcspec(self, shared_florestad_node):
        """Verify error responses match the JSON-RPC spec structure."""
        resp = shared_florestad_node.rpc.noraise_raw_request(
            {
                "jsonrpc": "2.0",
                "id": "struct_err",
                "method": "nonexistent",
                "params": [],
            },
        )
        body = resp["body"]
        assert "error" in body
        assert "id" in body
        assert body["id"] == "struct_err"
        err = body["error"]
        assert "code" in err
        assert "message" in err
        assert isinstance(err["code"], int)

    def test_jsonrpc_v1_explicit_version_succeeds(self, shared_florestad_node):
        """Verify requests with explicit jsonrpc 1.0 version succeed."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
            {"jsonrpc": "1.0", "id": "test", "method": "getblockcount", "params": []},
        )

    def test_jsonrpc_v1_omitted_version_succeeds(self, shared_florestad_node):
        """Verify requests without jsonrpc field succeed (JSON-RPC 1.0 style)."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
            {"id": "test", "method": "getblockcount"}
        )

    def test_contenttype_applicationjson_succeeds(self, shared_florestad_node):
        """Verify requests with application/json content-type succeed."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
            {"jsonrpc": "2.0", "id": "test", "method": "getblockcount"},
            content_type="application/json",
        )

    def test_contenttype_textplain_succeeds(self, shared_florestad_node):
        """Verify requests with text/plain content-type succeed."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_success(
            {"jsonrpc": "2.0", "id": "test", "method": "getblockcount"},
            content_type="text/plain",
        )

    def test_contenttype_nonjson_body_rejected(self, shared_florestad_node):
        """Verify non-JSON body is rejected regardless of content-type."""
        shared_florestad_node.rpc.ensure_rpc_raw_request_call_error(
            payload="this is not json",
            content_type="text/plain",
            expected_status_code=400,
        )
