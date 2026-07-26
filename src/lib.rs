use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
}

#[derive(Clone, Debug, Default)]
struct EntryDraft {
    line: u32,
    obsolete: bool,
    children: Vec<ChildDraft>,
}

struct PoParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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

    for (index, raw) in source.lines().enumerate() {
        let line = index as u32;
        total_lines = line;
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

        let comments = parse_comments(statement, line);
        if !comments.is_empty() {
            current.children.extend(comments);
            last_field_index = None;
            continue;
        }
        if let Some(child) = parse_message_field(statement, line) {
            current.children.push(child);
            last_field_index = current.children.len().checked_sub(1);
            continue;
        }
        if statement.starts_with('"') {
            if let Some(field_index) = last_field_index {
                let continuation = unquote(statement);
                current.children[field_index].label.push_str(&continuation);
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
        0,
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
    node(id, node_type, &label, draft.line, 0, &children)
}

fn child_node(id: &str, draft: &ChildDraft) -> SemanticNode {
    node(
        id,
        draft.node_type,
        &draft.label,
        draft.line,
        draft.col,
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

fn parse_comments(statement: &str, line: u32) -> Vec<ChildDraft> {
    if let Some(rest) = statement.strip_prefix("#,") {
        return rest
            .split(',')
            .filter_map(|flag| {
                let label = flag.trim();
                (!label.is_empty()).then(|| ChildDraft {
                    node_type: "flag",
                    label: label.to_string(),
                    line,
                    col: 0,
                })
            })
            .collect();
    }
    if let Some(rest) = statement.strip_prefix("#:") {
        return rest
            .split_whitespace()
            .map(|reference| ChildDraft {
                node_type: "reference",
                label: reference.to_string(),
                line,
                col: 0,
            })
            .collect();
    }
    let (node_type, rest) = if let Some(rest) = statement.strip_prefix("#.") {
        ("extracted_comment", rest)
    } else if let Some(rest) = statement.strip_prefix("#") {
        ("translator_comment", rest)
    } else {
        return Vec::new();
    };
    let label = rest.trim().to_string();
    if label.is_empty() {
        Vec::new()
    } else {
        vec![ChildDraft {
            node_type,
            label,
            line,
            col: 0,
        }]
    }
}

fn parse_message_field(statement: &str, line: u32) -> Option<ChildDraft> {
    for (prefix, node_type) in [
        ("msgctxt", "msgctxt"),
        ("msgid_plural", "msgid_plural"),
        ("msgid", "msgid"),
        ("msgstr[", "msgstr_plural"),
        ("msgstr", "msgstr"),
    ] {
        if statement.starts_with(prefix) {
            return Some(ChildDraft {
                node_type,
                label: unquote(statement),
                line,
                col: statement.find('"').unwrap_or_default() as u32,
            });
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

fn node(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    col: u32,
    children: &[SemanticNode],
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line,
        col,
        line,
        col + label.len() as u32,
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
        intentdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
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
}
