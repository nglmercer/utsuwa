//! Typed tools: JSON handling once, in an adapter, instead of in every
//! tool implementation.
//!
//! [`TypedTool`] keeps `tool-core::Tool` as the runtime ABI. The adapter
//! ([`TypedToolAdapter`]) owns the repetitive edge: reject non-object
//! args, deserialize `Args`, delegate capability reporting (best-effort
//! when args fail to parse), call, then serialize `Output` into
//! [`ToolOutput`](tool_core::ToolOutput).
//!
//! Argument structs are the schema source of truth: implementors return
//! their JSON Schema from [`TypedTool::input_schema`]. A future `schemars`
//! dependency can generate it; until then it is written once, next to the
//! struct, instead of drifting from separate parsing code.
//!
//! Migration is incremental: new tools use `TypedTool`; existing `Tool`
//! impls keep working indefinitely.

use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

/// A tool with typed arguments and output.
#[async_trait::async_trait]
pub trait TypedTool: Send + Sync {
    type Args: DeserializeOwned + Send;
    type Output: Serialize + Send;

    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for `Args`, kept next to the struct it describes.
    fn input_schema(&self) -> serde_json::Value;
    fn effects(&self) -> Vec<tool_core::ToolEffect>;

    fn required_capability(&self, _args: &Self::Args) -> Option<CapabilityRequirement> {
        None
    }

    async fn call(&self, ctx: ToolContext, args: Self::Args) -> Result<Self::Output, ToolError>;
}

/// Adapter exposing any [`TypedTool`] as a runtime [`Tool`].
pub struct TypedToolAdapter<T: TypedTool>(pub T);

impl<T: TypedTool> TypedToolAdapter<T> {
    pub fn new(tool: T) -> Self {
        Self(tool)
    }

    pub fn arc(tool: T) -> Arc<Self> {
        Arc::new(Self(tool))
    }

    fn invalid_args(&self, message: impl Into<String>) -> ToolError {
        ToolError::InvalidArgs {
            tool: self.0.id().to_string(),
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
impl<T: TypedTool> Tool for TypedToolAdapter<T> {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(self.0.id()),
            description: self.0.description().to_string(),
            input_schema: self.0.input_schema(),
            effects: self.0.effects(),
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        // Best-effort: unparsable args report no capability so the call
        // proceeds to `invoke`, where deserialization fails loudly instead
        // of silently passing policy with a guessed resource.
        let parsed: T::Args = serde_json::from_value(args.clone()).ok()?;
        self.0.required_capability(&parsed)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        if !args.is_object() {
            return Err(self.invalid_args("args must be a JSON object"));
        }
        let parsed: T::Args =
            serde_json::from_value(args).map_err(|e| self.invalid_args(e.to_string()))?;
        let output = self.0.call(ctx, parsed).await?;
        let content =
            serde_json::to_value(&output).map_err(|e| self.invalid_args(e.to_string()))?;
        Ok(ToolOutput::new(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};

    #[derive(serde::Deserialize)]
    struct EchoArgs {
        text: Option<String>,
    }

    #[derive(serde::Serialize, PartialEq, Debug)]
    struct EchoOut {
        text: String,
    }

    struct Echo;

    #[async_trait::async_trait]
    impl TypedTool for Echo {
        type Args = EchoArgs;
        type Output = EchoOut;

        fn id(&self) -> &'static str {
            "test.echo"
        }

        fn description(&self) -> &'static str {
            "typed echo"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
            })
        }

        fn effects(&self) -> Vec<tool_core::ToolEffect> {
            vec![tool_core::ToolEffect::ReadOnly]
        }

        async fn call(
            &self,
            _ctx: ToolContext,
            args: Self::Args,
        ) -> Result<Self::Output, ToolError> {
            Ok(EchoOut {
                text: args.text.unwrap_or_default(),
            })
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(Principal::Agent(AgentId::new("typed-test")))
    }

    #[tokio::test]
    async fn typed_round_trip_serializes_output() {
        let tool = TypedToolAdapter::new(Echo);
        assert_eq!(tool.metadata().id.0, "test.echo");
        let out = tool
            .invoke(ctx(), serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out.content, serde_json::json!({"text": "hi"}));
    }

    #[tokio::test]
    async fn non_object_and_bad_shape_are_invalid_args() {
        let tool = TypedToolAdapter::new(Echo);
        assert!(matches!(
            tool.invoke(ctx(), serde_json::json!([1])).await,
            Err(ToolError::InvalidArgs { .. })
        ));
        assert!(matches!(
            tool.invoke(ctx(), serde_json::json!({"text": 1})).await,
            Err(ToolError::InvalidArgs { .. })
        ));
    }

    #[test]
    fn unparsable_args_yield_no_capability_claim() {
        let tool = TypedToolAdapter::new(Echo);
        assert!(tool
            .required_capability(&serde_json::json!({"text": 1}))
            .is_none());
        assert!(tool.required_capability(&serde_json::json!({})).is_none());
    }
}
