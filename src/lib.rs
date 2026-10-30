use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "po";
const ROOT_NODE_TYPE: &str = "po_catalog";
const DETECT_EXTENSIONS: &[&str] = &[".po", ".pot"];
const DEFAULT_OLD: &str = "msgid \"hello\"\nmsgstr \"Hello\"\n";
const DEFAULT_NEW: &str = "msgid \"hello\"\nmsgstr \"Hi\"\n";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

#[derive(Clone, Debug)]
struct ChildDraft {
    node_type: &'static str,
    label: String,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
}

#[derive(Clone, Debug, Default)]
struct EntryDraft {
    line: u32,
    obsolete: bool,
    children: Vec<ChildDraft>,
}

struct PoParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let lower = filename.to_lowercase();
    if DETECT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

fn parse_po(source: &str) -> String {
    let mut entries = Vec::new();
    let mut current = EntryDraft::default();
    let mut last_field_index: Option<usize> = None;
    let mut total_lines = 0u32;
    let mut last_col = 0u32;

    for (index, raw) in source.lines().enumerate() {
        let line = index as u32;
        total_lines = line;
        last_col = raw.len() as u32; // UTF-8 byte columns, not Unicode scalar counts.
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            flush_entry(&mut current, &mut entries);
            last_field_index = None;
            continue;
        }

        let (obsolete, statement) = if let Some(rest) = trimmed.strip_prefix("#~") {
            (true, rest.trim_start())
        } else {
            (false, trimmed)
        };
        // The trimmed/obsolete statement is a slice of the original source line.
        let statement_col = raw.find(statement).unwrap_or_default() as u32;
        if obsolete {
            current.obsolete = true;
        }
        if current.children.is_empty() {
            current.line = line;
        }

        if starts_new_message(statement, &current) {
            flush_entry(&mut current, &mut entries);
            current.line = line;
            current.obsolete = obsolete;
        }

        let comments = parse_comments(statement, line, statement_col);
        if !comments.is_empty() {
            current.children.extend(comments);
            last_field_index = None;
            continue;
        }
        if let Some(child) = parse_message_field(statement, line, statement_col) {
            current.children.push(child);
            last_field_index = current.children.len().checked_sub(1);
            continue;
        }
        if statement.starts_with('"') {
            if let Some(field_index) = last_field_index {
                let continuation = unquote(statement);
                let child = &mut current.children[field_index];
                child.label.push_str(&continuation);
                // A continued PO string occupies additional source lines. Its
                // displayed/decoded label length cannot stand in for byte spans.
                let last_quote = statement.rfind('"').filter(|&end| end > 0).unwrap_or(statement.len());
                child.end_line = line;
                child.end_col = statement_col + last_quote as u32;
            }
        }
    }
    flush_entry(&mut current, &mut entries);

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        last_col,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &entries),
    )
    .children(entries)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

fn starts_new_message(statement: &str, current: &EntryDraft) -> bool {
    statement.starts_with("msgid ")
        && current
            .children
            .iter()
            .any(|child| matches!(child.node_type, "msgid" | "msgstr" | "msgstr_plural"))
}

fn flush_entry(current: &mut EntryDraft, entries: &mut Vec<SemanticNode>) {
    if current.children.is_empty() {
        current.obsolete = false;
        return;
    }
    let id = format!("0.{}", entries.len());
    let entry = entry_node(&id, current);
    entries.push(entry);
    *current = EntryDraft::default();
}

fn entry_node(id: &str, draft: &EntryDraft) -> SemanticNode {
    let children: Vec<SemanticNode> = draft
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| child_node(&format!("{id}.{index}"), child))
        .collect();
    let mut label = entry_label(&children);
    if draft.obsolete {
        label = format!("obsolete: {label}");
    }
    let node_type = if draft.obsolete {
        "obsolete_message"
    } else {
        "message"
    };
    // The entry label omits PO syntax and can be shorter than the bytes edited
    // in its children. Cover every original-source child, including msgstr.
    let (end_line, end_col) = draft
        .children
        .iter()
        .map(|child| (child.end_line, child.end_col))
        .max()
        .unwrap_or((draft.line, 0));
    node_span(id, node_type, &label, draft.line, 0, end_line, end_col, &children)
}

fn child_node(id: &str, draft: &ChildDraft) -> SemanticNode {
    node_span(
        id,
        draft.node_type,
        &draft.label,
        draft.line,
        draft.col,
        draft.end_line,
        draft.end_col,
        &[],
    )
}

fn entry_label(children: &[SemanticNode]) -> String {
    let context = first_label(children, "msgctxt");
    let msgid = first_label(children, "msgid")
        .or_else(|| first_label(children, "msgstr"))
        .or_else(|| first_label(children, "msgstr_plural"))
        .unwrap_or_else(|| "message".to_string());
    if let Some(context) = context {
        format!("{context}: {msgid}")
    } else {
        msgid
    }
}

fn first_label(children: &[SemanticNode], node_type: &str) -> Option<String> {
    children
        .iter()
        .find(|child| child.node_type == node_type)
        .map(|child| child.label.clone())
}

fn child_draft(
    node_type: &'static str,
    label: String,
    line: u32,
    col: u32,
    end_col: u32,
) -> ChildDraft {
    ChildDraft { node_type, label, line, col, end_line: line, end_col }
}

fn parse_comments(statement: &str, line: u32, statement_col: u32) -> Vec<ChildDraft> {
    if let Some(rest) = statement.strip_prefix("#,") {
        let mut consumed = 0usize;
        return rest
            .split(',')
            .filter_map(|piece| {
                let piece_offset = consumed;
                consumed += piece.len() + 1;
                let label = piece.trim();
                if label.is_empty() { return None; }
                let col = statement_col + 2 + (piece_offset + piece.find(label).unwrap_or_default()) as u32;
                Some(child_draft("flag", label.to_string(), line, col, col + label.len() as u32))
            })
            .collect();
    }
    if let Some(rest) = statement.strip_prefix("#:") {
        let mut cursor = 0usize;
        return rest
            .split_whitespace()
            .map(|reference| {
                let offset = cursor + rest[cursor..].find(reference).unwrap_or_default();
                cursor = offset + reference.len();
                let col = statement_col + 2 + offset as u32;
                child_draft("reference", reference.to_string(), line, col, col + reference.len() as u32)
            })
            .collect();
    }
    let (node_type, rest) = if let Some(rest) = statement.strip_prefix("#.") {
        ("extracted_comment", rest)
    } else if let Some(rest) = statement.strip_prefix('#') {
        ("translator_comment", rest)
    } else {
        return Vec::new();
    };
    let label = rest.trim();
    if label.is_empty() {
        Vec::new()
    } else {
        let col = statement_col + statement.find(label).unwrap_or_default() as u32;
        vec![child_draft(node_type, label.to_string(), line, col, col + label.len() as u32)]
    }
}

fn parse_message_field(statement: &str, line: u32, statement_col: u32) -> Option<ChildDraft> {
    for (prefix, node_type) in [
        ("msgctxt", "msgctxt"),
        ("msgid_plural", "msgid_plural"),
        ("msgid", "msgid"),
        ("msgstr[", "msgstr_plural"),
        ("msgstr", "msgstr"),
    ] {
        if statement.starts_with(prefix) {
            let (start, end) = if let Some(open) = statement.find('"') {
                // The content, not its decoded label, is the original-source
                // region. In particular, the closing quote is the half-open
                // end even for escaped content or multibyte UTF-8 characters.
                let end = statement.rfind('"').filter(|&close| close > open).unwrap_or(statement.len());
                (open + 1, end)
            } else {
                (0, statement.len())
            };
            let col = statement_col + start as u32;
            return Some(child_draft(node_type, unquote(statement), line, col, statement_col + end as u32));
        }
    }
    None
}

fn unquote(statement: &str) -> String {
    let Some(start) = statement.find('"') else {
        return statement.trim().to_string();
    };
    let Some(end) = statement.rfind('"') else {
        return statement[start + 1..].to_string();
    };
    if end <= start {
        return String::new();
    }
    let mut value = String::new();
    let mut chars = statement[start + 1..end].chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('"') => value.push('"'),
                Some('\\') => value.push('\\'),
                Some(other) => value.push(other),
                None => value.push('\\'),
            }
        } else {
            value.push(ch);
        }
    }
    value
}

fn node_span(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
    children: &[SemanticNode],
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line,
        col,
        end_line,
        end_col,
        stable_hash(node_type, label, children),
    )
    .children(children.to_vec())
    .build()
}

fn stable_hash(node_type: &str, label: &str, children: &[SemanticNode]) -> String {
    let mut value = format!("{node_type}:{label}");
    for child in children {
        value.push('|');
        value.push_str(&child.structural_hash);
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

impl Guest for PoParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        parse_po(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(PoParser);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_by_type(node: &SemanticNode, node_type: &str, labels: &mut Vec<String>) {
        if node.node_type == node_type {
            labels.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, labels);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(PoParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn grammar_id_is_language_id() {
        assert_eq!(PoParser::grammar_id(), LANGUAGE_ID);
        assert_eq!(PoParser::language_ids(), vec![LANGUAGE_ID.to_string()]);
    }

    #[test]
    fn detects_po_extensions() {
        assert_eq!(
            detect_language_impl("messages.po", DEFAULT_NEW),
            LANGUAGE_ID
        );
        assert_eq!(
            detect_language_impl("template.pot", DEFAULT_NEW),
            LANGUAGE_ID
        );
    }

    #[test]
    fn process_returns_valid_json() {
        let parsed = parse_po(DEFAULT_NEW);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
    }

    #[test]
    fn process_extracts_context_plural_flags_references_and_obsolete_entries() {
        let parsed = parse_po(
            r#"
#. Button label
#: src/ui.py:10 templates/base.html:2
#, fuzzy, python-format
msgctxt "button"
msgid "Save %s"
msgid_plural "Save %s files"
msgstr[0] "Save %s"
msgstr[1] "Save %s files"

#~ msgid "Old label"
#~ msgstr "Ancien"
"#,
        );
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut messages = Vec::new();
        let mut obsolete = Vec::new();
        let mut contexts = Vec::new();
        let mut ids = Vec::new();
        let mut plurals = Vec::new();
        let mut flags = Vec::new();
        let mut references = Vec::new();
        labels_by_type(&root, "message", &mut messages);
        labels_by_type(&root, "obsolete_message", &mut obsolete);
        labels_by_type(&root, "msgctxt", &mut contexts);
        labels_by_type(&root, "msgid", &mut ids);
        labels_by_type(&root, "msgid_plural", &mut plurals);
        labels_by_type(&root, "flag", &mut flags);
        labels_by_type(&root, "reference", &mut references);

        assert!(messages.contains(&"button: Save %s".to_string()));
        assert!(obsolete.contains(&"obsolete: Old label".to_string()));
        assert!(contexts.contains(&"button".to_string()));
        assert!(ids.contains(&"Save %s".to_string()));
        assert!(plurals.contains(&"Save %s files".to_string()));
        assert!(flags.contains(&"fuzzy".to_string()));
        assert!(flags.contains(&"python-format".to_string()));
        assert!(references.contains(&"src/ui.py:10".to_string()));
        assert!(references.contains(&"templates/base.html:2".to_string()));
    }

    #[test]
    fn source_spans_cover_last_edited_po_string_byte() {
        let old: serde_json::Value =
            serde_json::from_str(&parse_po("msgid \"greeting\"\n")).unwrap();
        let new: serde_json::Value =
            serde_json::from_str(&parse_po("msgid \"greetingX\"\n")).unwrap();
        let old_message = &old["children"][0];
        let new_message = &new["children"][0];
        let new_id = &new_message["children"][0];
        assert_eq!(old_message["position"]["end_col"], 15);
        assert_eq!(new_message["position"]["start_col"], 0);
        assert_eq!(new_message["position"]["end_col"], 16);
        assert_eq!(new_id["position"]["start_col"], 7);
        assert_eq!(new_id["position"]["end_col"], 16);
        assert_eq!(new_id["label"], "greetingX");
    }

    #[test]
    fn source_spans_use_utf8_bytes_and_preserve_indentation() {
        let source = "  msgid \"hélloX\"\n  msgstr \"Hi\"\n";
        let root: serde_json::Value = serde_json::from_str(&parse_po(source)).unwrap();
        let message = &root["children"][0];
        let id = &message["children"][0];
        assert_eq!(id["label"], "hélloX");
        assert_eq!(id["position"]["start_line"], 0);
        assert_eq!(id["position"]["start_col"], 9);
        assert_eq!(id["position"]["end_col"], 16);
        assert_eq!(message["position"]["end_line"], 1);
        assert_eq!(message["position"]["end_col"], 12);
        assert_eq!(root["position"]["end_line"], 1);
        assert_eq!(root["position"]["end_col"], 13);
    }

    #[test]
    fn source_spans_follow_continuation_bytes_not_decoded_lengths() {
        let source = "msgid \"\"\n\"héllo\"\nmsgstr \"\"\n\"Bon\\nX\"\n";
        let root: serde_json::Value = serde_json::from_str(&parse_po(source)).unwrap();
        let message = &root["children"][0];
        let id = &message["children"][0];
        let translation = &message["children"][1];
        assert_eq!(id["label"], "héllo");
        assert_eq!(id["position"]["start_line"], 0);
        assert_eq!(id["position"]["end_line"], 1);
        assert_eq!(id["position"]["end_col"], 7);
        assert_eq!(translation["position"]["end_line"], 3);
        assert_eq!(translation["position"]["end_col"], 7);
        assert_eq!(message["position"]["end_line"], 3);
        assert_eq!(message["position"]["end_col"], 7);
    }

}
