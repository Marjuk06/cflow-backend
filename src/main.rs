use axum::{routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tree_sitter::{Node, Parser};
use tower_http::cors::{Any, CorsLayer};
use std::fs;
use std::process::{Command, Stdio};
use std::io::Write;
use std::env;
use std::net::SocketAddr;

#[derive(Deserialize)]
struct CodePayload {
    code: String,
    #[serde(default)]
    stdin: String,
}

#[derive(Serialize, Clone)]
struct FlowNode { id: String, label: String, kind: String }

#[derive(Serialize, Clone)]
struct FlowEdge { id: String, source: String, target: String, label: String }

#[derive(Serialize)]
struct ParseResult { nodes: Vec<FlowNode>, edges: Vec<FlowEdge> }

#[derive(Serialize)]
struct ExecuteResult { output: String, error: String }

#[derive(Clone)]
struct ActivePath { node_id: String, label: String }

/// Passed down through recursion so break/continue know where to jump
#[derive(Clone)]
struct LoopContext {
    header_id: String,      // for `continue` — jump back to loop condition
    break_sink_id: String,  // for `break`    — jump to loop exit
}

struct GraphBuilder {
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    id_counter: usize,
    extra_edges: Vec<FlowEdge>, // back-edges, break/continue jumps
}

impl GraphBuilder {
    fn new() -> Self {
        Self { nodes: Vec::new(), edges: Vec::new(), id_counter: 0, extra_edges: Vec::new() }
    }

    fn next_id(&mut self) -> String {
        let id = format!("node-{}", self.id_counter);
        self.id_counter += 1;
        id
    }

    fn clean_condition(&self, text: &str) -> String {
        text.trim_matches(|c| c == '(' || c == ')' || c == ' ').to_string()
    }

    fn connect_paths(&mut self, from: &[ActivePath], to: &str) {
        for path in from {
            self.edges.push(FlowEdge {
                id: format!("e-{}-{}", path.node_id, to),
                source: path.node_id.clone(),
                target: to.to_string(),
                label: path.label.clone(),
            });
        }
    }

    fn add_extra_edge(&mut self, source: &str, target: &str, label: &str) {
        let id = format!("x-{}-{}-{}", source, target, self.extra_edges.len());
        self.extra_edges.push(FlowEdge {
            id,
            source: source.to_string(),
            target: target.to_string(),
            label: label.to_string(),
        });
    }

    fn process_statement<'a>(
        &mut self,
        statement: Node<'a>,
        payload: &str,
        active_paths: Vec<ActivePath>,
        loop_ctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        let kind = statement.kind();

        // Skip pure syntax tokens
        if ["{", "}", "comment", ";", "\n", "preproc_include", "preproc_def"].contains(&kind) {
            return active_paths;
        }

        // Skip no-op declarations (no assignment)
        if kind == "declaration" {
            let text = &payload[statement.start_byte()..statement.end_byte()];
            if !text.contains('=') { return active_paths; }
        }

        // Compound block → recurse children
        if kind == "compound_statement" {
            let mut cursor = statement.walk();
            let mut paths = active_paths;
            for child in statement.children(&mut cursor) {
                paths = self.process_statement(child, payload, paths, loop_ctx);
            }
            return paths;
        }

        // else_clause → recurse body (skip "else" token)
        if kind == "else_clause" {
            let mut cursor = statement.walk();
            let mut paths = active_paths;
            for child in statement.children(&mut cursor) {
                if child.kind() == "else" || child.kind() == "comment" { continue; }
                paths = self.process_statement(child, payload, paths, loop_ctx);
            }
            return paths;
        }

        // RETURN → terminal, path ends
        if kind == "return_statement" {
            let nid = self.next_id();
            let code = payload[statement.start_byte()..statement.end_byte()].to_string();
            self.nodes.push(FlowNode { id: nid.clone(), label: code, kind: "return_statement".into() });
            self.connect_paths(&active_paths, &nid);
            return vec![];
        }

        // BREAK → jump to break sink
        if kind == "break_statement" {
            if let Some(ctx) = loop_ctx {
                for path in &active_paths {
                    self.add_extra_edge(&path.node_id, &ctx.break_sink_id, &path.label);
                }
            }
            return vec![];
        }

        // CONTINUE → jump back to loop header
        if kind == "continue_statement" {
            if let Some(ctx) = loop_ctx {
                for path in &active_paths {
                    self.add_extra_edge(&path.node_id, &ctx.header_id, &path.label);
                }
            }
            return vec![];
        }

        if kind == "if_statement"     { return self.process_if(statement, payload, active_paths, loop_ctx); }
        if kind == "while_statement"  { return self.process_while(statement, payload, active_paths, loop_ctx); }
        if kind == "for_statement"    { return self.process_for(statement, payload, active_paths, loop_ctx); }
        if kind == "do_statement"     { return self.process_do_while(statement, payload, active_paths, loop_ctx); }
        if kind == "switch_statement" { return self.process_switch(statement, payload, active_paths, loop_ctx); }

        // Default: normal process node
        let nid = self.next_id();
        let code = payload[statement.start_byte()..statement.end_byte()].to_string();
        self.nodes.push(FlowNode { id: nid.clone(), label: code, kind: kind.to_string() });
        self.connect_paths(&active_paths, &nid);
        vec![ActivePath { node_id: nid, label: "".into() }]
    }

    // ── IF / ELSE-IF / ELSE ──
    fn process_if<'a>(
        &mut self, statement: Node<'a>, payload: &str,
        active_paths: Vec<ActivePath>, loop_ctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        let did = self.next_id();
        let cond = statement.child_by_field_name("condition").unwrap();
        let cond_text = self.clean_condition(&payload[cond.start_byte()..cond.end_byte()]);
        self.nodes.push(FlowNode { id: did.clone(), label: cond_text, kind: "decision".into() });
        self.connect_paths(&active_paths, &did);

        let mut out = Vec::new();
        if let Some(cons) = statement.child_by_field_name("consequence") {
            let yes = vec![ActivePath { node_id: did.clone(), label: "Yes".into() }];
            out.extend(self.process_statement(cons, payload, yes, loop_ctx));
        }
        if let Some(alt) = statement.child_by_field_name("alternative") {
            let no = vec![ActivePath { node_id: did.clone(), label: "No".into() }];
            out.extend(self.process_statement(alt, payload, no, loop_ctx));
        } else {
            out.push(ActivePath { node_id: did.clone(), label: "No".into() });
        }
        out
    }

    // ── WHILE ──
    fn process_while<'a>(
        &mut self, statement: Node<'a>, payload: &str,
        active_paths: Vec<ActivePath>, _lctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        // Condition decision
        let hid = self.next_id();
        let cond = statement.child_by_field_name("condition").unwrap();
        let cond_text = self.clean_condition(&payload[cond.start_byte()..cond.end_byte()]);
        self.nodes.push(FlowNode { id: hid.clone(), label: cond_text, kind: "decision".into() });
        self.connect_paths(&active_paths, &hid);

        // Break-sink connector (loop exit)
        let bsid = self.next_id();
        self.nodes.push(FlowNode { id: bsid.clone(), label: "".into(), kind: "connector".into() });

        let ictx = LoopContext { header_id: hid.clone(), break_sink_id: bsid.clone() };

        // Body
        let yes = vec![ActivePath { node_id: hid.clone(), label: "Yes".into() }];
        if let Some(body) = statement.child_by_field_name("body") {
            let exits = self.process_statement(body, payload, yes, Some(&ictx));
            // Back-edges: body exits → header
            for p in &exits { self.add_extra_edge(&p.node_id, &hid, &p.label); }
        }

        // No → exit
        self.add_extra_edge(&hid, &bsid, "No");
        vec![ActivePath { node_id: bsid, label: "".into() }]
    }

    // ── FOR ──
    fn process_for<'a>(
        &mut self, statement: Node<'a>, payload: &str,
        active_paths: Vec<ActivePath>, _lctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        // 1. Init node
        let mut after_init = active_paths;
        if let Some(init) = statement.child_by_field_name("initializer") {
            let raw = payload[init.start_byte()..init.end_byte()]
                .trim_end_matches(';').trim().to_string();
            if !raw.is_empty() {
                let nid = self.next_id();
                self.nodes.push(FlowNode { id: nid.clone(), label: raw, kind: "expression_statement".into() });
                self.connect_paths(&after_init, &nid);
                after_init = vec![ActivePath { node_id: nid, label: "".into() }];
            }
        }

        // 2. Condition (decision)
        let hid = self.next_id();
        let cond_label = statement.child_by_field_name("condition")
            .map(|c| payload[c.start_byte()..c.end_byte()].trim_end_matches(';').trim().to_string())
            .unwrap_or_else(|| "true".into());
        self.nodes.push(FlowNode { id: hid.clone(), label: cond_label, kind: "decision".into() });
        self.connect_paths(&after_init, &hid);

        // 3. Break-sink
        let bsid = self.next_id();
        self.nodes.push(FlowNode { id: bsid.clone(), label: "".into(), kind: "connector".into() });

        // 4. Update node
        let uid_opt: Option<String> = statement.child_by_field_name("update").map(|upd| {
            let raw = payload[upd.start_byte()..upd.end_byte()].trim().to_string();
            let nid = self.next_id();
            self.nodes.push(FlowNode { id: nid.clone(), label: raw, kind: "expression_statement".into() });
            nid
        });

        let ictx = LoopContext { header_id: hid.clone(), break_sink_id: bsid.clone() };

        // 5. Body
        let yes = vec![ActivePath { node_id: hid.clone(), label: "Yes".into() }];
        if let Some(body) = statement.child_by_field_name("body") {
            let exits = self.process_statement(body, payload, yes, Some(&ictx));
            if let Some(ref uid) = uid_opt {
                for p in &exits { self.add_extra_edge(&p.node_id, uid, &p.label); }
                self.add_extra_edge(uid, &hid, "");
            } else {
                for p in &exits { self.add_extra_edge(&p.node_id, &hid, &p.label); }
            }
        }

        // 6. No → exit
        self.add_extra_edge(&hid, &bsid, "No");
        vec![ActivePath { node_id: bsid, label: "".into() }]
    }

    // ── DO-WHILE ──
    fn process_do_while<'a>(
        &mut self, statement: Node<'a>, payload: &str,
        active_paths: Vec<ActivePath>, _lctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        let bsid = self.next_id();
        self.nodes.push(FlowNode { id: bsid.clone(), label: "".into(), kind: "connector".into() });

        let hid = self.next_id();
        let cond = statement.child_by_field_name("condition").unwrap();
        let cond_text = self.clean_condition(&payload[cond.start_byte()..cond.end_byte()]);
        self.nodes.push(FlowNode { id: hid.clone(), label: cond_text, kind: "decision".into() });

        let ictx = LoopContext { header_id: hid.clone(), break_sink_id: bsid.clone() };

        if let Some(body) = statement.child_by_field_name("body") {
            let exits = self.process_statement(body, payload, active_paths, Some(&ictx));
            self.connect_paths(&exits, &hid);
        }

        // Body entry node (we need a connector to loop back to)
        let body_start_id = self.next_id();
        self.nodes.push(FlowNode { id: body_start_id.clone(), label: "".into(), kind: "connector".into() });
        self.add_extra_edge(&hid, &body_start_id, "Yes"); // Yes → repeat body
        self.add_extra_edge(&hid, &bsid, "No");

        vec![ActivePath { node_id: bsid, label: "".into() }]
    }

    // ── SWITCH ──
    fn process_switch<'a>(
        &mut self, statement: Node<'a>, payload: &str,
        active_paths: Vec<ActivePath>, _lctx: Option<&LoopContext>,
    ) -> Vec<ActivePath> {
        let sid = self.next_id();
        let val = statement.child_by_field_name("value").unwrap();
        let val_text = payload[val.start_byte()..val.end_byte()].to_string();
        self.nodes.push(FlowNode { id: sid.clone(), label: format!("switch ({})", val_text), kind: "decision".into() });
        self.connect_paths(&active_paths, &sid);

        let eid = self.next_id();
        self.nodes.push(FlowNode { id: eid.clone(), label: "".into(), kind: "connector".into() });

        let ictx = LoopContext { header_id: sid.clone(), break_sink_id: eid.clone() };

        let body = match statement.child_by_field_name("body") {
            Some(b) => b,
            None => return vec![ActivePath { node_id: eid, label: "".into() }],
        };

        let mut cursor = body.walk();
        let children: Vec<Node> = body.children(&mut cursor).collect();

        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            if child.kind() != "case_statement" { i += 1; continue; }

            // Case label
            let case_label = if let Some(v) = child.child_by_field_name("value") {
                format!("Case {}", payload[v.start_byte()..v.end_byte()].to_string())
            } else {
                "Default".to_string()
            };

            let entry = vec![ActivePath { node_id: sid.clone(), label: case_label }];
            let mut case_paths = entry;

            // Process statements inside this case
            let mut inner = child.walk();
            for stmt in child.children(&mut inner) {
                let sk = stmt.kind();
                if sk == "case" || sk == "default" || sk == ":" { continue; }
                case_paths = self.process_statement(stmt, payload, case_paths, Some(&ictx));
            }

            // Fall-through paths connect to exit
            for path in &case_paths {
                self.add_extra_edge(&path.node_id, &eid, &path.label);
            }
            i += 1;
        }

        vec![ActivePath { node_id: eid, label: "".into() }]
    }
}

// ─────────────────────────────────────────────
// AXUM SERVER
// ─────────────────────────────────────────────
#[tokio::main]
async fn main() {
    // 1. Setup the Router (The 'app' Railway is looking for)
    let app = Router::new()
        .route("/parse", post(handle_parse)) // Make sure 'handle_parse' is the name of your function
        .layer(CorsLayer::permissive());     // Allows your frontend to talk to this backend

    // 2. Setup the Port and Address
    let port_str = env::var("PORT").unwrap_or_else(|_| "3001".to_string());
    let port: u16 = port_str.parse().expect("PORT must be a number");
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    println!("Listening on http://{}", addr);

    // 3. Start the Server
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap(); 
}

async fn parse_c_code(Json(payload): Json<CodePayload>) -> Json<ParseResult> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).unwrap();
    let tree = parser.parse(&payload.code, None).unwrap();

    let mut builder = GraphBuilder::new();
    let mut cursor = tree.root_node().walk();
    let mut current_paths = vec![ActivePath { node_id: "start".into(), label: "".into() }];

    for child in tree.root_node().children(&mut cursor) {
        if child.kind() == "function_definition" {
            for i in 0..child.child_count() {
                let func_part = child.child(i as u32).unwrap();
                if func_part.kind() == "compound_statement" {
                    let mut ic = func_part.walk();
                    for stmt in func_part.children(&mut ic) {
                        current_paths = builder.process_statement(stmt, &payload.code, current_paths, None);
                    }
                }
            }
        }
    }

    let end_id = "end".to_string();
    builder.nodes.push(FlowNode { id: end_id.clone(), label: "End".into(), kind: "terminal".into() });
    for path in current_paths {
        builder.edges.push(FlowEdge {
            id: format!("e-{}-end", path.node_id),
            source: path.node_id,
            target: end_id.clone(),
            label: "".into(),
        });
    }

    // Merge extra back-edges
    builder.edges.extend(builder.extra_edges.clone());

    Json(ParseResult { nodes: builder.nodes, edges: builder.edges })
}

async fn execute_c_code(Json(payload): Json<CodePayload>) -> Json<ExecuteResult> {
    let uid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().subsec_nanos();
    let file_name = format!("temp_{}.c", uid);
    let exe_name  = if cfg!(windows) { format!("temp_{}.exe", uid) } else { format!("./temp_{}", uid) };

    if let Err(e) = fs::write(&file_name, &payload.code) {
        return Json(ExecuteResult { output: "".into(), error: format!("Failed to write: {}", e) });
    }

    let compile = Command::new("gcc").arg(&file_name).arg("-o").arg(&exe_name).output();
    match compile {
        Ok(out) if !out.status.success() => {
            let _ = fs::remove_file(&file_name);
            return Json(ExecuteResult { output: "".into(), error: String::from_utf8_lossy(&out.stderr).into() });
        }
        Err(e) => {
            let _ = fs::remove_file(&file_name);
            return Json(ExecuteResult { output: "".into(), error: format!("GCC not found: {}", e) });
        }
        _ => {}
    }

    let mut child = match Command::new(&exe_name)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_file(&file_name); let _ = fs::remove_file(&exe_name);
            return Json(ExecuteResult { output: "".into(), error: format!("Run failed: {}", e) });
        }
    };

    if let Some(mut sin) = child.stdin.take() {
        if !payload.stdin.is_empty() {
            let _ = sin.write_all(payload.stdin.as_bytes());
            let _ = sin.write_all(b"\n");
        }
    }

    let _ = fs::remove_file(&file_name);
    match child.wait_with_output() {
        Ok(out) => {
            let _ = fs::remove_file(&exe_name);
            Json(ExecuteResult {
                output: String::from_utf8_lossy(&out.stdout).into(),
                error:  String::from_utf8_lossy(&out.stderr).into(),
            })
        }
        Err(e) => Json(ExecuteResult { output: "".into(), error: format!("Execution failed: {}", e) }),
    }
}