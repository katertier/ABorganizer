//! Conversational dispatcher scaffolding (ADR-0032, slice B.11).
//!
//! Defines the tool-call envelope, per-tool args types, and the
//! [`Tool`] trait + [`ToolRegistry`] that future LLM integration
//! (Phase D.4) will dispatch through. No FM bridge wiring here —
//! that's `aborg ask` / `POST /api/v1/ask` work in their own slice
//! once the surrounding pieces (saved queries, dashboard, etc.)
//! are in place.
//!
//! ## Why a scaffold first
//!
//! Two consumers want this shape pinned ahead of the dispatcher's
//! own debut:
//!
//! * Saved queries (ADR-0034 / B.12) persist a `QueryFilter` JSON;
//!   the dispatcher emits the same shape via `ListBooks.args`.
//! * The Apple FM `complete_structured` bridge (ADR-0018) needs a
//!   stable `JsonSchema`-friendly enum to constrain LLM output.
//!
//! ## Tool envelope
//!
//! `serde(tag = "tool", rename_all = "snake_case")` so a JSON
//! payload like `{"tool": "list_books", "args": {…},
//! "confirmation_required": false, "rationale": "…"}` deserialises
//! straight into [`ToolCall::ListBooks`].
//!
//! Mutating tools default `confirmation_required: true`; read tools
//! default `false`. The defaults are enforced by the per-tool
//! [`Tool::is_mutating`] hook so adding a new tool can't silently
//! ship without the dry-run posture.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened with the dispatcher slice

use std::collections::BTreeMap;
use std::sync::Arc;

use ab_query::QueryFilter;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One LLM-emitted tool call envelope.
///
/// `args` shape is per-tool. Mutating tools (those with
/// `is_mutating() == true`) default `confirmation_required: true`
/// — the dispatcher refuses to commit without an explicit operator
/// "yes" turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolCall {
    ListBooks {
        args: QueryFilter,
        #[serde(default)]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    Search {
        args: SearchArgs,
        #[serde(default)]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    ShowBook {
        args: ShowBookArgs,
        #[serde(default)]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    RetryStage {
        args: RetryArgs,
        #[serde(default = "default_confirm_required")]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    Clean {
        args: CleanArgs,
        #[serde(default = "default_confirm_required")]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    AudiologoCut {
        args: CutArgs,
        #[serde(default = "default_confirm_required")]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    Explain {
        args: ExplainArgs,
        #[serde(default)]
        confirmation_required: bool,
        #[serde(default)]
        rationale: String,
    },
    Clarify {
        question: String,
        #[serde(default)]
        candidates: Vec<ToolCandidate>,
    },
}

const fn default_confirm_required() -> bool {
    true
}

/// One option presented in a [`ToolCall::Clarify`] round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCandidate {
    pub label: String,
    /// Nested tool call the operator can accept by name.
    pub call: Box<ToolCall>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShowBookArgs {
    pub book_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetryArgs {
    pub book_id: i64,
    #[serde(default)]
    pub stages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanArgs {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CutArgs {
    pub book_id: i64,
    pub head_ms: u32,
    pub tail_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainArgs {
    /// Free-form follow-up like "why was this DNF?" routed to the
    /// LLM with current book context.
    pub question: String,
    #[serde(default)]
    pub book_id: Option<i64>,
}

/// Outcome of dispatching one tool call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub tool: &'static str,
    pub success: bool,
    /// Operator-facing one-liner. Structured payload (if any) lives
    /// in `data`.
    pub summary: String,
    pub data: serde_json::Value,
    /// `true` when the call ran in dry-run mode (`commit = false`).
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("tool refused: {0}")]
    Refused(String),
    #[error(
        "confirmation required for mutating tool {0}; resend with confirmation_required=false after operator approval"
    )]
    ConfirmationRequired(&'static str),
    #[error(transparent)]
    Other(#[from] ab_core::Error),
}

/// State threaded into every tool invocation.
///
/// The shape stays generic over the SQL pool so consumers can wire
/// the production pools when the dispatcher slice (Phase D.4) lands
/// without forcing this crate to take a hard dependency on `ab-db`.
#[derive(Clone)]
pub struct DispatcherCtx {
    pub library: sqlx::SqlitePool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    /// `true` for tools that modify state. Used by the dispatcher
    /// to enforce the dry-run posture (mutating tools must carry
    /// `confirmation_required: true` until an operator yes-turn
    /// flips the flag).
    fn is_mutating(&self) -> bool;

    /// Run the tool. `commit = false` is dry-run; the tool must
    /// not write state and should return a preview-shaped
    /// `ToolResult` with `dry_run: true`.
    async fn invoke(
        &self,
        ctx: &DispatcherCtx,
        call: &ToolCall,
        commit: bool,
    ) -> Result<ToolResult, DispatchError>;
}

/// Cheap-to-clone registry. Tools are looked up by `tool` tag.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<&'static str, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut map: BTreeMap<&'static str, Arc<dyn Tool>> = BTreeMap::new();
        for t in tools {
            map.insert(t.name(), t);
        }
        Self {
            tools: Arc::new(map),
        }
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Extract the `tool` tag from a [`ToolCall`] without owning it.
#[must_use]
pub const fn tool_name(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::ListBooks { .. } => "list_books",
        ToolCall::Search { .. } => "search",
        ToolCall::ShowBook { .. } => "show_book",
        ToolCall::RetryStage { .. } => "retry_stage",
        ToolCall::Clean { .. } => "clean",
        ToolCall::AudiologoCut { .. } => "audiologo_cut",
        ToolCall::Explain { .. } => "explain",
        ToolCall::Clarify { .. } => "clarify",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn list_books_round_trips() {
        let call = ToolCall::ListBooks {
            args: QueryFilter {
                q: Some("kings".into()),
                ..Default::default()
            },
            confirmation_required: false,
            rationale: "matched 'show me' shape".into(),
        };
        let json = serde_json::to_string(&call).expect("ser");
        assert!(json.contains(r#""tool":"list_books""#));
        let back: ToolCall = serde_json::from_str(&json).expect("de");
        assert_eq!(tool_name(&back), "list_books");
    }

    #[test]
    fn retry_stage_defaults_confirmation_required_to_true() {
        let json = r#"{"tool":"retry_stage","args":{"book_id":42}}"#;
        let call: ToolCall = serde_json::from_str(json).expect("de");
        match call {
            ToolCall::RetryStage {
                confirmation_required,
                ..
            } => assert!(
                confirmation_required,
                "mutating tool must default to confirmation-required"
            ),
            _ => panic!("expected RetryStage"),
        }
    }

    #[test]
    fn list_books_defaults_confirmation_required_to_false() {
        let json = r#"{"tool":"list_books","args":{}}"#;
        let call: ToolCall = serde_json::from_str(json).expect("de");
        match call {
            ToolCall::ListBooks {
                confirmation_required,
                ..
            } => assert!(!confirmation_required, "read tool defaults false"),
            _ => panic!("expected ListBooks"),
        }
    }

    #[test]
    fn clarify_carries_candidates() {
        let json = r#"{"tool":"clarify","question":"which series?","candidates":[
            {"label":"Mistborn","call":{"tool":"list_books","args":{"series":"Mistborn"}}}
        ]}"#;
        let call: ToolCall = serde_json::from_str(json).expect("de");
        match call {
            ToolCall::Clarify { candidates, .. } => assert_eq!(candidates.len(), 1),
            _ => panic!("expected Clarify"),
        }
    }

    #[test]
    fn unknown_tool_rejected_at_deserialize() {
        let json = r#"{"tool":"explode","args":{}}"#;
        let r: Result<ToolCall, _> = serde_json::from_str(json);
        assert!(r.is_err());
    }

    #[test]
    fn registry_lookup_by_name() {
        struct DummyList;
        #[async_trait]
        impl Tool for DummyList {
            fn name(&self) -> &'static str {
                "list_books"
            }
            fn is_mutating(&self) -> bool {
                false
            }
            async fn invoke(
                &self,
                _ctx: &DispatcherCtx,
                _call: &ToolCall,
                _commit: bool,
            ) -> Result<ToolResult, DispatchError> {
                Ok(ToolResult {
                    tool: "list_books",
                    success: true,
                    summary: "ok".into(),
                    data: serde_json::Value::Null,
                    dry_run: false,
                })
            }
        }
        let r = ToolRegistry::new(vec![Arc::new(DummyList)]);
        assert_eq!(r.names(), vec!["list_books"]);
        assert!(r.get("list_books").is_some());
        assert!(r.get("nonexistent").is_none());
    }
}
