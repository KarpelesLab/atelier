//! Agent loop: conversation state and turn orchestration.
//!
//! M1: the tool-call loop (model -> tool_calls -> execute -> feed results ->
//! repeat until the turn produces no tool calls). Empty until then.
