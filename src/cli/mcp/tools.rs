//! Strict tool arguments and discoverable schemas share the existing operations.
use super::super::control_session::Operation;
use crate::core::events::query::{EventFilter, EventQuery};
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
pub(super) fn empty(args: Value) -> Result<(), ErrorData> {
    decode::<Empty>(args).map(|_| ())
}
fn decode<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ErrorData> {
    serde_json::from_value(args).map_err(|e| ErrorData::invalid_params(e.to_string(), None))
}

pub(super) fn operation(name: &str, args: Value) -> Result<Operation, ErrorData> {
    if name == "devd_events" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Events {
            #[serde(default = "tail")]
            tail: usize,
            service: Option<String>,
        }
        fn tail() -> usize {
            100
        }
        let args: Events = decode(args)?;
        let query = EventQuery {
            tail: args.tail,
            filter: EventFilter {
                service: args.service,
                ..Default::default()
            },
            cursor: None,
        };
        query
            .validate()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        return Ok(Operation::Events { query });
    }
    if name == "devd_incidents_list" {
        empty(args)?;
        return Ok(Operation::Incidents { id: None });
    }
    if name == "devd_incidents_show" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Show {
            id: String,
        }
        let args: Show = decode(args)?;
        let id = super::super::incidents::query::parse_id(&args.id)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        return Ok(Operation::Incidents { id: Some(id) });
    }
    let method = match name {
        "devd_identity" => "identity",
        "devd_status" => "status",
        "devd_explain" => "explain",
        "devd_wait" => "wait",
        "devd_export" => "export",
        "devd_restart" => "restart",
        "devd_stop" => "stop",
        "devd_reload_preview" => "reload-preview",
        "devd_reload_apply" => "reload-apply",
        "devd_clean_preview" => "clean-preview",
        "devd_clean_apply" => "clean-apply",
        _ => return Err(ErrorData::invalid_params("unknown tool", None)),
    };
    let Value::Object(mut args) = args else {
        return Err(ErrorData::invalid_params(
            "arguments must be an object",
            None,
        ));
    };
    if args.contains_key("method") {
        return Err(ErrorData::invalid_params(
            "method is not a tool argument",
            None,
        ));
    }
    if name == "devd_wait" {
        args.entry("timeout_ms").or_insert(json!(30_000));
    }
    args.insert("method".into(), method.into());
    decode(Value::Object(args))
}

pub(super) fn result(value: Value) -> CallToolResult {
    // Text-only JSON keeps the evidence intact for legacy and modern clients
    // without duplicating multi-MiB reports in structuredContent.
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}
pub(super) fn failure(code: &str, message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        json!({"code":code,"message":message}).to_string(),
    )])
}

pub(super) fn catalog(offline: bool) -> Vec<Tool> {
    let string = json!({"type":"string"});
    if offline {
        return vec![tool("devd_diagnose", "Diagnose the launcher's report; all evidence is historical, current health is unknown.", json!({}), &[], false)];
    }
    let target = json!({"type":"object","properties":{"instance_id":string,"run_id":string},"required":["instance_id","run_id"],"additionalProperties":false});
    let operation_key = json!({"type":"string","minLength":1,"maxLength":128});
    vec![
        tool("devd_identity", "Query the attached live instance and run identity, including control_policy limits and remaining restarts when the launcher enables a policy.", json!({}), &[], false),
        tool("devd_status", "Query live service states without commands or environment values.", json!({}), &[], false),
        tool("devd_events", "Read retained events with explicit history gaps.", json!({"tail":{"type":"integer","minimum":1,"maximum":1000,"default":100},"service":string}), &[], false),
        tool("devd_explain", "Explain one service using recorded causes, generations and gaps.", json!({"service":string}), &["service"], false),
        tool("devd_wait", "Wait for strict readiness; cancellation only stops waiting.", json!({"services":{"type":"array","items":string},"timeout_ms":{"type":"integer","minimum":1,"maximum":3600000,"default":30000}}), &[], false),
        tool("devd_export", "Return a diagnostic report; include_logs explicitly adds up to 200 application log entries. Does not write files.", json!({"include_logs":{"type":"boolean","default":false}}), &[], false),
        tool("devd_incidents_list", "List retained incidents of this run with coverage gaps.", json!({}), &[], false),
        tool("devd_incidents_show", "Read an incident's recorded evidence; does not re-inspect its paths.", json!({"id":string}), &["id"], false),
        tool("devd_restart", "Restart a service. Requires launcher restart grant, matching target, and operation_key when deduplication is enabled; retry the same key after a lost reply.", json!({"service":string,"target":target,"operation_key":operation_key}), &["service","target"], true),
        tool("devd_stop", "Request ordered stack stop. Requires launcher stop grant and matching target; operation_key is required when deduplication is enabled.", json!({"target":target,"operation_key":operation_key}), &["target"], true),
        tool("devd_reload_preview", "Preview configuration changes and obtain a plan ID.", json!({"candidate":string}), &[], false),
        tool("devd_reload_apply", "Apply a fresh reload plan. Requires launcher reload grant and matching target; operation_key is required when deduplication is enabled. Failure stops the stack; no rollback.", json!({"candidate":string,"plan_id":string,"target":target,"operation_key":operation_key}), &["plan_id","target"], true),
        tool("devd_clean_preview", "Preview authorized disposable directories; removal requires normal stack shutdown.", json!({}), &[], false),
        tool("devd_clean_apply", "Delete only registered disposable directories after normal shutdown. Requires launcher clean grant, matching target and fresh plan; operation_key is required when deduplication is enabled.", json!({"plan_id":string,"target":target,"operation_key":operation_key}), &["plan_id","target"], true),
    ]
}
fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    control: bool,
) -> Tool {
    let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    let mut tool = Tool::new(name, description, schema.as_object().unwrap().clone());
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(!control)
            .destructive(control)
            .idempotent(!control)
            .open_world(false),
    );
    tool
}
