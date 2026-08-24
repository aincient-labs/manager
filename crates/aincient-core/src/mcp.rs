//! `atelier mcp` — a stdio MCP server for the pack developer's coding agent
//! (plans/byo-components.md W9).
//!
//! Every tool is a thin proxy over an appliance endpoint that exists ONLY in
//! dev mode (`atelier pack dev` sets AINCIENT_DEV, opening /atelier/dev/*) —
//! so the agent works against ground truth: the compiled catalog, the real
//! admission gate, the exact prompt text the page agent sees. The one local
//! tool is `scaffold_component`, which writes files into the pack in the
//! working directory. The tool contract rides the pack metadata's `api: 1`.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdio (the MCP stdio
//! framing), blocking — matching the rest of this crate, no async runtime.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::pack::{self, Pack};
use crate::stack::Stack;

/// Serve MCP over stdin/stdout until stdin closes. `port` is the appliance's
/// published HTTP port (the stack's HTTP_PORT).
pub fn serve(stack: &Stack) -> Result<()> {
    let port = stack.http_port();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // not JSON — nothing sane to answer
        };
        // Notifications (no id) get no response, per JSON-RPC.
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    // Echo the client's protocol version — this server's
                    // surface (tools only) is valid under every published rev.
                    "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05"),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "atelier", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Ground-truth tools for developing an Atelier CMS component pack against the LIVE dev appliance (`atelier pack dev` must be running). Validate with pack_validate after every component.yml change; read prompt_manifest to see exactly what the page agent sees; prove placement with agent_eval (needs an AI provider connected)."
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tool_list() } }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match call_tool(port, name, &args) {
                    Ok((text, is_error)) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": text }], "isError": is_error }
                    }),
                    Err(e) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

/// The published tool contract (versioned by the pack metadata's api major).
fn tool_list() -> Value {
    let kind_arg = json!({ "type": "object", "properties": {
        "kind": { "type": "string", "description": "Page kind (e.g. landing, blog). Default: landing." }
    }});
    json!([
        {
            "name": "catalog",
            "description": "The compiled component catalog for a page kind — exactly what the site's renderer, validator and page agent read (components, tiers, props, tones, variants, stylesheets).",
            "inputSchema": kind_arg
        },
        {
            "name": "pack_validate",
            "description": "Run the appliance's admission gate + CSS lint + atelier.pack.yml check. THE check to run after editing a component: a component this rejects is excluded from the catalog at boot.",
            "inputSchema": { "type": "object", "properties": {
                "module": { "type": "string", "description": "Pack module machine name. Omit to check every atelier component on the site." }
            }}
        },
        {
            "name": "kind_check",
            "description": "Dry-run the current catalog against every stored page: which slots a kind/constraint/component change would orphan (safe vs breaking). Report only, never rewrites.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "prompt_manifest",
            "description": "The EXACT palette text inlined into the page agent's system prompt for a kind, with char/approx-token counts (budget: keep it lean — every extra component is prompt cost).",
            "inputSchema": kind_arg
        },
        {
            "name": "design_tokens",
            "description": "The design-token contract pack CSS must route through (var(--…)), plus the generated token/utility preset CSS a pack build imports.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "render_example",
            "description": "Render one of a component's declared examples through the REAL page pipeline and CSS; returns standalone HTML. Use it to check markup/tokens before looking at the gallery.",
            "inputSchema": { "type": "object", "required": ["component"], "properties": {
                "component": { "type": "string", "description": "Component machine name." },
                "example": { "type": "integer", "description": "Index into the component's examples list. Default 0." },
                "tone": { "type": "string", "description": "Tone override (e.g. inverted) to test legibility on other surfaces." }
            }}
        },
        {
            "name": "agent_eval",
            "description": "Ask the site's REAL page agent model to compose against a kind's live palette, graded by the live catalog: would it place your component, with valid props, honouring the kind's opener? THE check for 'the agent won't use my component'. Needs an AI provider connected on the appliance.",
            "inputSchema": { "type": "object", "required": ["ask"], "properties": {
                "ask": { "type": "string", "description": "The user request to evaluate, e.g. 'add a spotlight for our spring launch'." },
                "kind": { "type": "string", "description": "Page kind to evaluate against. Default: landing." },
                "expect": { "type": "string", "description": "Component machine name that must appear in the placed sections for the eval to pass." }
            }}
        },
        {
            "name": "scaffold_component",
            "description": "Add a new component skeleton (component.yml + twig with the atelier metadata block) to the pack in the current working directory. Local file write; validate with pack_validate after filling it in.",
            "inputSchema": { "type": "object", "required": ["name"], "properties": {
                "name": { "type": "string", "description": "New component machine name (lowercase, digits, _)." }
            }}
        }
    ])
}

/// Execute one tool. Returns (text, is_error).
fn call_tool(port: u16, name: &str, args: &Value) -> Result<(String, bool)> {
    let kind = || {
        args.get("kind")
            .and_then(Value::as_str)
            .unwrap_or("landing")
            .to_string()
    };
    let get = |path: String| -> Result<(String, bool)> {
        let (status, body) = pack::http_get(port, &path)?;
        if status == 403 {
            return Ok((format!("HTTP 403 from {path} — the appliance is not in dev mode. Start it with `atelier pack dev` (it sets AINCIENT_DEV).\n{body}"), true));
        }
        Ok((body, status >= 400))
    };
    match name {
        "catalog" => get(format!("/atelier/dev/catalog?kind={}", kind())),
        "pack_validate" => {
            let module = args.get("module").and_then(Value::as_str).unwrap_or("");
            get(format!("/atelier/dev/pack-validate?module={module}"))
        }
        "kind_check" => get("/atelier/dev/kind-check".into()),
        "prompt_manifest" => get(format!("/atelier/dev/prompt-manifest?kind={}", kind())),
        "design_tokens" => get("/atelier/dev/design-tokens".into()),
        "render_example" => {
            let component = args
                .get("component")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let example = args.get("example").and_then(Value::as_i64).unwrap_or(0);
            let tone = args.get("tone").and_then(Value::as_str).unwrap_or("");
            let mut path = format!("/atelier/dev/render?component={component}&example={example}");
            if !tone.is_empty() {
                path.push_str(&format!("&tone={tone}"));
            }
            get(path)
        }
        "agent_eval" => {
            let ask = args.get("ask").and_then(Value::as_str).unwrap_or_default();
            if ask.is_empty() {
                return Ok(("agent_eval needs an `ask` (the user request to evaluate).".into(), true));
            }
            let mut path = format!(
                "/atelier/dev/agent-eval?kind={}&ask={}",
                kind(),
                percent_encode(ask)
            );
            if let Some(expect) = args.get("expect").and_then(Value::as_str) {
                if !expect.is_empty() {
                    path.push_str(&format!("&expect={}", percent_encode(expect)));
                }
            }
            get(path)
        }
        "scaffold_component" => {
            let new = args.get("name").and_then(Value::as_str).unwrap_or_default();
            scaffold_component(&std::env::current_dir()?, new)
        }
        _ => Ok((format!("unknown tool: {name}"), true)),
    }
}

/// Minimal query-string percent-encoding (RFC 3986 unreserved pass-through) —
/// enough for the free-text `ask` without pulling a url crate in.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Write a component skeleton into the pack rooted at `dir`.
fn scaffold_component(dir: &Path, name: &str) -> Result<(String, bool)> {
    if !pack::valid_module_name(name) {
        return Ok((format!("\"{name}\" is not a valid component machine name (lowercase, digits, _)"), true));
    }
    let pack = Pack::locate(dir)?;
    let dest = pack.dir.join("components").join(name);
    if dest.exists() {
        return Ok((format!("{} already exists — refusing to overwrite", dest.display()), true));
    }
    std::fs::create_dir_all(&dest)?;
    let label = {
        let mut l = name.replace('_', " ");
        let upper = l[0..1].to_uppercase();
        l.replace_range(0..1, &upper);
        l
    };
    let yml = include_str!("../templates/pack/showcase.component.yml.tpl")
        .replace("__MODULE__", &pack.module)
        .replace("__LABEL__", &label)
        .replace("Showcase", &label)
        .replace("showcase", name);
    let twig = include_str!("../templates/pack/showcase.twig.tpl").replace("showcase", name);
    std::fs::write(dest.join(format!("{name}.component.yml")), yml)?;
    std::fs::write(dest.join(format!("{name}.twig")), twig)?;
    Ok((
        format!(
            "scaffolded components/{name}/ ({name}.component.yml + {name}.twig). Fill in `use:` (the agent's ONLY selection signal), the props and an `examples:` entry, then run pack_validate."
        ),
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_is_well_formed() {
        let tools = tool_list();
        let tools = tools.as_array().unwrap();
        assert_eq!(tools.len(), 8);
        for tool in tools {
            assert!(tool.get("name").is_some());
            assert!(tool.get("description").is_some());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn scaffold_component_writes_the_skeleton() {
        let tmp = std::env::temp_dir().join(format!("atelier-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("acme_pack.info.yml"), "name: Acme\n").unwrap();
        let (msg, is_err) = scaffold_component(&tmp, "banner_wide").unwrap();
        assert!(!is_err, "{msg}");
        let yml = std::fs::read_to_string(tmp.join("components/banner_wide/banner_wide.component.yml")).unwrap();
        assert!(yml.contains("stylesheet: assets/acme_pack.css"));
        assert!(!yml.contains("showcase"));
        let (_, is_err) = scaffold_component(&tmp, "banner_wide").unwrap();
        assert!(is_err, "second scaffold must refuse");
        let (_, is_err) = scaffold_component(&tmp, "Bad-Name").unwrap();
        assert!(is_err);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
