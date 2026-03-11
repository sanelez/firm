//! LSP server implementation for Firm DSL.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use log::info;
use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use firm_core::{Entity, EntityType};
use firm_core::schema::EntitySchema;
use firm_lang::diagnostics::{self, Diagnostic, DiagnosticSeverity, SourceSpan};
use firm_lang::parser::dsl::parse_source;
use firm_lang::workspace::Workspace;

use crate::completion;

/// Cached workspace data used for completions.
pub struct WorkspaceData {
    pub schemas: HashMap<EntityType, EntitySchema>,
    pub entities: Vec<Entity>,
}

/// The Firm language server.
pub struct FirmLspServer {
    client: Client,
    workspace_path: PathBuf,
    workspace_data: Arc<RwLock<Option<WorkspaceData>>>,
    /// In-memory document text cache, updated on open/change.
    documents: Arc<RwLock<HashMap<String, String>>>,
}

impl FirmLspServer {
    pub fn new(client: Client, workspace_path: PathBuf) -> Self {
        Self {
            client,
            workspace_path,
            workspace_data: Arc::new(RwLock::new(None)),
            documents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start the language server on stdio.
    pub async fn serve_stdio(workspace_path: PathBuf) -> std::result::Result<(), String> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let (service, socket) =
            LspService::new(|client| FirmLspServer::new(client, workspace_path));
        Server::new(stdin, stdout, socket).serve(service).await;
        Ok(())
    }

    /// Publish diagnostics for a single document.
    async fn publish_diagnostics(&self, uri: Uri, text: &str) {
        let file_path = uri_to_path(&uri).unwrap_or_default();

        let parsed = match parse_source(text.to_string(), Some(file_path)) {
            Ok(parsed) => parsed,
            Err(e) => {
                log::error!("Failed to parse source: {e}");
                return;
            }
        };

        let firm_diagnostics = diagnostics::collect_syntax_errors(&parsed);
        let lsp_diagnostics: Vec<tower_lsp_server::lsp_types::Diagnostic> = firm_diagnostics
            .iter()
            .map(to_lsp_diagnostic)
            .collect();

        self.client
            .publish_diagnostics(uri, lsp_diagnostics, None)
            .await;
    }

    /// Load the full workspace and publish workspace-level diagnostics.
    ///
    /// Groups diagnostics by file and publishes per-document so the LSP client
    /// shows them on the correct files. Also updates the cached workspace data
    /// for completions.
    async fn publish_workspace_diagnostics(&self) {
        let mut workspace = Workspace::new();
        if let Err(e) = workspace.load_directory(&self.workspace_path) {
            log::error!("Failed to load workspace for diagnostics: {e}");
            return;
        }

        // Update cached workspace data for completions (best-effort)
        let data = build_workspace_data(&workspace);
        *self.workspace_data.write().await = Some(data);

        // Collect syntax errors first
        let mut has_syntax_errors = false;
        let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
        for parsed in workspace.parsed_sources() {
            let syntax_errors = diagnostics::collect_syntax_errors(parsed);
            if !syntax_errors.is_empty() {
                has_syntax_errors = true;
            }
            all_diagnostics.extend(syntax_errors);
        }

        // Only run workspace diagnostics if there are no syntax errors
        if !has_syntax_errors {
            all_diagnostics.extend(diagnostics::collect_workspace_diagnostics(&workspace));
        }

        // Group diagnostics by file path and publish per document
        let mut by_file: std::collections::HashMap<PathBuf, Vec<Diagnostic>> =
            std::collections::HashMap::new();
        for diag in all_diagnostics {
            by_file
                .entry(diag.span.file.clone())
                .or_default()
                .push(diag);
        }

        for (file_path, file_diagnostics) in by_file {
            let full_path = self.workspace_path.join(&file_path);
            let uri_string = format!("file://{}", full_path.display());
            if let Ok(uri) = uri_string.parse::<Uri>() {
                let lsp_diagnostics: Vec<tower_lsp_server::lsp_types::Diagnostic> =
                    file_diagnostics.iter().map(to_lsp_diagnostic).collect();
                self.client
                    .publish_diagnostics(uri, lsp_diagnostics, None)
                    .await;
            }
        }
    }

}

impl LanguageServer for FirmLspServer {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        info!("Firm language server initializing");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("Firm language server initialized");
        self.client
            .log_message(MessageType::INFO, "Firm language server ready")
            .await;

        // Build workspace data in background so completions work immediately
        let workspace_data = self.workspace_data.clone();
        let workspace_path = self.workspace_path.clone();
        tokio::spawn(async move {
            let mut workspace = Workspace::new();
            if let Err(e) = workspace.load_directory(&workspace_path) {
                log::error!("Failed to load workspace on startup: {e}");
                return;
            }
            let data = build_workspace_data(&workspace);
            info!("Startup: cached {} schemas, {} entities for completions",
                data.schemas.len(), data.entities.len());
            *workspace_data.write().await = Some(data);
        });
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Firm language server shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();
        self.documents
            .write()
            .await
            .insert(uri.as_str().to_string(), text);
        self.publish_diagnostics(params.text_document.uri, &params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync mode: last content change has the full text
        if let Some(change) = params.content_changes.last() {
            self.documents
                .write()
                .await
                .insert(
                    params.text_document.uri.as_str().to_string(),
                    change.text.clone(),
                );
            self.publish_diagnostics(params.text_document.uri, &change.text)
                .await;
        }
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        // On save, run full workspace diagnostics (includes syntax + workspace-level).
        // This reloads all files from disk for a consistent view.
        self.publish_workspace_diagnostics().await;
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let file_path = match uri_to_path(uri) {
            Some(p) => p,
            None => return Ok(None),
        };

        // Use in-memory document text (reflects unsaved edits), fall back to disk
        let docs = self.documents.read().await;
        let text = match docs.get(uri.as_str()) {
            Some(t) => {
                log::info!("completion: using cached document text ({} chars)", t.len());
                t.clone()
            }
            None => {
                log::info!("completion: no cached text for {}, reading from disk", uri.as_str());
                match std::fs::read_to_string(&file_path) {
                    Ok(t) => t,
                    Err(_) => return Ok(None),
                }
            }
        };
        drop(docs);

        // Parse with tree-sitter
        let parsed = match parse_source(text.clone(), Some(file_path)) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        // Detect cursor context using tree-sitter
        let tree = &parsed.tree;
        let point = tree_sitter::Point::new(position.line as usize, position.character as usize);
        let root = tree.root_node();

        // Find the node at cursor position
        let cursor_node = root.descendant_for_point_range(point, point);

        // Read cached workspace data
        let data_guard = self.workspace_data.read().await;
        let data = match data_guard.as_ref() {
            Some(d) => d,
            None => {
                log::info!("completion: no workspace data cached yet");
                return Ok(None);
            }
        };

        // Determine completion context by walking up from cursor node
        if let Some(node) = cursor_node {
            let line_text = text.lines().nth(position.line as usize).unwrap_or("");
            log::info!(
                "completion: pos=({},{}) node_kind={:?} line={:?}",
                position.line, position.character, node.kind(), line_text
            );
            let context = detect_completion_context(node, &text, position);

            match context {
                CompletionContext::FieldName { ref entity_type, ref existing_fields } => {
                    log::info!("completion: context=FieldName entity_type={entity_type} existing={existing_fields:?}");
                    let items = completion::complete_field_names(
                        entity_type,
                        &existing_fields.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &data.schemas,
                    );
                    return Ok(Some(CompletionResponse::Array(items)));
                }
                CompletionContext::Reference { ref prefix } => {
                    log::info!("completion: context=Reference prefix={prefix:?}");
                    let items = completion::complete_references(prefix, &data.entities);
                    return Ok(Some(CompletionResponse::Array(items)));
                }
                CompletionContext::FieldValue { ref entity_type, ref field_name } => {
                    log::info!("completion: context=FieldValue entity_type={entity_type} field_name={field_name}");
                    let items = completion::complete_enum_values(
                        entity_type,
                        field_name,
                        &data.schemas,
                    );
                    log::info!("completion: enum items count={}", items.len());
                    if !items.is_empty() {
                        return Ok(Some(CompletionResponse::Array(items)));
                    }
                }
                CompletionContext::None => {}
            }
        }

        Ok(None)
    }
}

/// Build workspace data (schemas + entities) from a loaded workspace.
/// Best-effort: skips any schemas or entities that fail conversion.
fn build_workspace_data(workspace: &Workspace) -> WorkspaceData {
    let mut schemas = HashMap::new();
    let mut entities = Vec::new();

    for parsed in workspace.parsed_sources() {
        for parsed_schema in parsed.schemas() {
            if let Ok(schema) = EntitySchema::try_from(&parsed_schema) {
                schemas.insert(schema.entity_type.clone(), schema);
            }
        }
        for parsed_entity in parsed.entities() {
            if let Ok(entity) = Entity::try_from(&parsed_entity) {
                entities.push(entity);
            }
        }
    }

    WorkspaceData { schemas, entities }
}

/// Completion context detected from cursor position.
enum CompletionContext {
    /// Cursor is in a field-name position inside an entity block.
    FieldName {
        entity_type: String,
        existing_fields: Vec<String>,
    },
    /// Cursor is in a reference value position (after a dot).
    Reference {
        prefix: String,
    },
    /// Cursor is in a value position for a field (after `=`, no dot).
    FieldValue {
        entity_type: String,
        field_name: String,
    },
    /// No actionable context detected.
    None,
}

/// Detect the completion context from a tree-sitter node and cursor position.
fn detect_completion_context(
    node: tree_sitter::Node,
    source: &str,
    position: Position,
) -> CompletionContext {
    // Get the line text up to cursor for reference detection
    let line_start = source.lines().nth(position.line as usize).unwrap_or("");
    let col = position.character as usize;
    let text_before_cursor = if col <= line_start.len() {
        &line_start[..col]
    } else {
        line_start
    };

    // Check if we're typing a reference (text before cursor contains a dot pattern)
    // e.g. "  contact = contact." or "  manager = person.ja"
    let trimmed = text_before_cursor.trim();

    // Text-based: if the line contains `=` and cursor is after it, we're in a value position.
    // This takes priority over tree-sitter node walking since incomplete text may not produce
    // the expected node types.
    if let Some(after_eq) = trimmed.rsplit_once('=') {
        let value_text = after_eq.1.trim();

        // If the value contains a dot, it's a reference completion
        if value_text.contains('.') {
            return CompletionContext::Reference {
                prefix: value_text.to_string(),
            };
        }

        // Otherwise it's a field value position — extract field name and entity type
        // for enum completions. Walk tree-sitter to find the enclosing entity_block.
        let field_name = after_eq.0.trim().to_string();
        if !field_name.is_empty() {
            let mut current = node;
            loop {
                if current.kind() == "entity_block" {
                    if let Some(entity_type) = extract_entity_type(current, source) {
                        return CompletionContext::FieldValue {
                            entity_type,
                            field_name,
                        };
                    }
                    break;
                }
                if current.kind() == "source_file" {
                    break;
                }
                match current.parent() {
                    Some(p) => current = p,
                    None => break,
                }
            }
        }
    }

    // Walk up to find entity_block context (field name completion)
    let mut current = node;
    loop {
        let kind = current.kind();
        if kind == "entity_block" {
            // We're inside an entity block — extract entity type and existing fields
            let entity_type = extract_entity_type(current, source);
            let existing_fields = extract_existing_fields(current, source);

            if let Some(entity_type) = entity_type {
                return CompletionContext::FieldName {
                    entity_type,
                    existing_fields,
                };
            }
            return CompletionContext::None;
        }
        if kind == "source_file" {
            break;
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    // Also check if cursor position falls within an entity_block's block range
    // (handles the case where tree-sitter places cursor on whitespace with no useful node)
    let cursor = root_walk_entity_blocks(node);
    for entity_block in &cursor {
        if let Some(block_node) = find_block_child(*entity_block) {
            let start = block_node.start_position();
            let end = block_node.end_position();
            let p = tree_sitter::Point::new(position.line as usize, position.character as usize);
            if p.row > start.row && (p.row < end.row || (p.row == end.row && p.column < end.column))
            {
                let entity_type = extract_entity_type(*entity_block, source);
                let existing_fields = extract_existing_fields(*entity_block, source);
                if let Some(entity_type) = entity_type {
                    return CompletionContext::FieldName {
                        entity_type,
                        existing_fields,
                    };
                }
            }
        }
    }

    CompletionContext::None
}

/// Walk to root and collect all entity_block nodes (for fallback position matching).
fn root_walk_entity_blocks(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;
    }
    // current is now root
    let mut result = Vec::new();
    let mut tree_cursor = current.walk();
    for child in current.children(&mut tree_cursor) {
        if child.kind() == "entity_block" {
            result.push(child);
        }
    }
    result
}

/// Find the `block` child of an entity_block node.
fn find_block_child(entity_block: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cursor = entity_block.walk();
    entity_block
        .children(&mut cursor)
        .find(|c| c.kind() == "block")
}

/// Extract the entity type from an entity_block node.
fn extract_entity_type(entity_block: tree_sitter::Node, source: &str) -> Option<String> {
    let mut cursor = entity_block.walk();
    for child in entity_block.children(&mut cursor) {
        if child.kind() == "entity_type" {
            return Some(get_node_text(&child, source).to_string());
        }
    }
    None
}

/// Extract existing field names from an entity_block node.
fn extract_existing_fields(entity_block: tree_sitter::Node, source: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let block = match find_block_child(entity_block) {
        Some(b) => b,
        None => return fields,
    };

    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        if child.kind() == "field" {
            let mut field_cursor = child.walk();
            for field_child in child.children(&mut field_cursor) {
                if field_child.kind() == "field_name" {
                    fields.push(get_node_text(&field_child, source).to_string());
                    break;
                }
            }
        }
    }
    fields
}

/// Get the text of a tree-sitter node.
fn get_node_text<'a>(node: &tree_sitter::Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// Convert a firm Diagnostic to an LSP Diagnostic.
fn to_lsp_diagnostic(d: &Diagnostic) -> tower_lsp_server::lsp_types::Diagnostic {
    tower_lsp_server::lsp_types::Diagnostic {
        range: span_to_range(&d.span),
        severity: Some(match d.severity {
            DiagnosticSeverity::Error => tower_lsp_server::lsp_types::DiagnosticSeverity::ERROR,
            DiagnosticSeverity::Warning => {
                tower_lsp_server::lsp_types::DiagnosticSeverity::WARNING
            }
        }),
        source: Some("firm".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// Convert a SourceSpan to an LSP Range.
fn span_to_range(span: &SourceSpan) -> Range {
    Range {
        start: Position {
            line: span.start_line,
            character: span.start_col,
        },
        end: Position {
            line: span.end_line,
            character: span.end_col,
        },
    }
}

/// Extract a file path from a document URI.
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    if let Some(path) = s.strip_prefix("file://") {
        Some(PathBuf::from(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse source, find node at position, run detect_completion_context.
    fn detect_context(source: &str, line: u32, character: u32) -> CompletionContext {
        let parsed = parse_source(source.to_string(), None).unwrap();
        let tree = &parsed.tree;
        let root = tree.root_node();
        let point = tree_sitter::Point::new(line as usize, character as usize);
        let node = root
            .descendant_for_point_range(point, point)
            .expect("no node at cursor position");

        log::info!(
            "Node at ({}, {}): kind={:?} text={:?}",
            line,
            character,
            node.kind(),
            &source[node.start_byte()..node.end_byte()]
        );

        detect_completion_context(node, source, Position { line, character })
    }

    #[test]
    fn test_context_field_name_on_empty_line_in_block() {
        let source = "project my_project {\n    title = \"hello\"\n    \n}";
        //                                                    ^ line 2, col 4
        let ctx = detect_context(source, 2, 4);
        match ctx {
            CompletionContext::FieldName { entity_type, .. } => {
                assert_eq!(entity_type, "project");
            }
            other => panic!("Expected FieldName, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_value_after_equals() {
        let source = "project my_project {\n    status = \n}";
        //                                            ^ line 1, col 13
        let ctx = detect_context(source, 1, 13);
        match ctx {
            CompletionContext::FieldValue {
                entity_type,
                field_name,
            } => {
                assert_eq!(entity_type, "project");
                assert_eq!(field_name, "status");
            }
            other => panic!("Expected FieldValue, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_value_after_equals_with_partial_text() {
        let source = "project my_project {\n    status = act\n}";
        //                                               ^ line 1, col 16
        let ctx = detect_context(source, 1, 16);
        match ctx {
            CompletionContext::FieldValue {
                entity_type,
                field_name,
            } => {
                assert_eq!(entity_type, "project");
                assert_eq!(field_name, "status");
            }
            other => panic!("Expected FieldValue, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_reference_after_dot() {
        let source = "project my_project {\n    contact = person.\n}";
        //                                                     ^ line 1, col 22
        let ctx = detect_context(source, 1, 22);
        match ctx {
            CompletionContext::Reference { prefix } => {
                assert_eq!(prefix, "person.");
            }
            other => panic!("Expected Reference, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_reference_field_after_second_dot() {
        let source = "project my_project {\n    contact = person.jane.\n}";
        //                                                           ^ line 1, col 27
        let ctx = detect_context(source, 1, 27);
        match ctx {
            CompletionContext::Reference { prefix } => {
                assert_eq!(prefix, "person.jane.");
            }
            other => panic!("Expected Reference, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_value_realistic_enum_typing() {
        // Simulates typing `status = e` inside an account entity
        let source = "account acme {\n    name = \"Acme\"\n    status = e\n}";
        //                                                          ^ line 2, col 15
        let ctx = detect_context(source, 2, 15);
        match ctx {
            CompletionContext::FieldValue {
                entity_type,
                field_name,
            } => {
                assert_eq!(entity_type, "account");
                assert_eq!(field_name, "status");
            }
            other => panic!("Expected FieldValue, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_value_cursor_right_after_equals_space() {
        // Simulates cursor right after `= ` with nothing typed yet
        let source = "account acme {\n    status = \n}";
        //                                        ^ line 1, col 13
        let ctx = detect_context(source, 1, 13);
        match ctx {
            CompletionContext::FieldValue {
                entity_type,
                field_name,
            } => {
                assert_eq!(entity_type, "account");
                assert_eq!(field_name, "status");
            }
            other => panic!("Expected FieldValue, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_value_with_existing_complete_field_above() {
        // Entity has one complete field, cursor on second field after =
        let source = "account acme {\n    name = \"Acme\"\n    status = \n}";
        //                                                            ^ line 2, col 13
        let ctx = detect_context(source, 2, 13);
        match ctx {
            CompletionContext::FieldValue {
                entity_type,
                field_name,
            } => {
                assert_eq!(entity_type, "account");
                assert_eq!(field_name, "status");
            }
            other => panic!("Expected FieldValue, got {:?}", context_name(&other)),
        }
    }

    #[test]
    fn test_context_field_name_no_equals_on_line() {
        // Just typing a new field name, no = yet
        let source = "account acme {\n    name = \"Acme\"\n    st\n}";
        //                                                     ^ line 2, col 6
        let ctx = detect_context(source, 2, 6);
        match ctx {
            CompletionContext::FieldName { entity_type, .. } => {
                assert_eq!(entity_type, "account");
            }
            other => panic!("Expected FieldName, got {:?}", context_name(&other)),
        }
    }

    fn context_name(ctx: &CompletionContext) -> &'static str {
        match ctx {
            CompletionContext::FieldName { .. } => "FieldName",
            CompletionContext::Reference { .. } => "Reference",
            CompletionContext::FieldValue { .. } => "FieldValue",
            CompletionContext::None => "None",
        }
    }
}
