//! Typed envelopes shared by Lu core modules. No JSON or MCP transport.

/// Internal typed-message envelope. This layer only carries the type URL and
/// bytes; it does not parse them or attest that they are a valid Protobuf
/// message. Schema validation belongs to the codec of each operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoPayload {
    pub type_url: String,
    pub bytes: Vec<u8>,
}

/// One requested operation, as recorded by the effect journal. Recording an
/// invocation is not permission to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub call_id: String,
    pub tool_id: String,
    pub workspace_id: Option<String>,
    pub input: ProtoPayload,
}
